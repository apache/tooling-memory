# Harness containers: what exists that we could use instead of our own

Written 20 October 2026, with the runs that needed `/dev/kvm`, a larger uid range and a real
login added on 23 October. The question: of what we built to confine a harness and route its
calls (`crates/sandbox`, the WSL containers backend, `images/harness`, `crates/egress`), what
could be replaced by existing work licensed Apache-2.0 or more permissively, and what should
stay ours.

Five layers were surveyed: embeddable confinement, external runtimes, the harness image, the
egress beside the box, and whole agent-sandbox platforms. Every licence below was read from
the repository or crate manifest on 19 or 20 October 2026 (`gh api repos/OWNER/REPO/license`,
and the LICENSE file where that said NOASSERTION), never from memory. The top candidates were
run on one machine against one check; [spikes/harness-containers](../spikes/harness-containers/)
holds the scripts and the raw results. Where something below was read but not run, it says so.

**In bounds**: Apache-2.0, MIT, BSD, ISC, Zlib, BSL-1.0, CC0, the Foundation's Category A.
**Noted, ranked out**: MPL-2.0 and EPL (Category B). **Out**: GPL, LGPL, AGPL, SSPL, BUSL,
proprietary, and open-core where the part we would need is the paid one. Executing a
separately installed LGPL binary is not linking it, but it is still a dependency we would
ship or require, so bubblewrap counts as out.

## The answer, short

Nothing replaces the wrapper. No project found combines what `crates/egress` does in one
place: one unix socket as the box's only way out, per-session token metering from streamed
responses, a label-versus-ceiling check before anything leaves, and memory served over MCP
on the same socket. Around it, three things are worth taking, and five of our own gaps
surfaced on the way:

| Action | What | Why |
|---|---|---|
| Fix | Refuse `AF_VSOCK` and `io_uring` with seccomp, set `no_new_privs`, in our Linux backend | The network namespace does not cover vsock; on WSL2 and any VM guest it reaches the host. Measured, below |
| Fix | Make the read-only system bind recursive (`mount_setattr` with `AT_RECURSIVE`) | Submounts under `/usr` stay writable inside our sandbox today; only file ownership holds them. Measured, below |
| Fix | Port-restrict the CONNECT tunnel and check the resolved address before dialling | `crates/egress/src/lib.rs:756-771` dials any port on an allowed name, wherever it resolves |
| Fix | Add `.github/dependabot.yml` | None exists; see the image section |
| Fixed here | The Seatbelt profile now lists the root directory and reads the system's tool selections (`609304d`, `535e356`). Still to fix: the macOS CI job does not compile | On macOS 26.6 every process under the old profile aborted at start, `/usr/bin/true` included. With the fix, the contract check passes every check under Seatbelt on GitHub's runner, below |
| Fix | Fail the launch when the egress socket cannot be bound, and say why | A root whose socket path passes 108 bytes launches anyway and fails on a bind mount instead. Measured, below |
| Adopt | `seccompiler` (Apache-2.0 OR BSD-3) and `landlock` (MIT OR Apache-2.0) in `crates/sandbox`, with Codex's seccomp rule set as the reference | The first fix, as libraries, from the rule set three harnesses already use |
| Adopt | gVisor as an optional hardened Linux backend | Passed every check here, rootless, no KVM, no daemon; the Node harnesses run under it, and a real Claude Code call completed through it |
| Adopt | An npm lockfile and a digest-pinned base for `images/harness`, bumped by Dependabot | Built here at no cost in size or time; pins what the harnesses pull in and the base, and retires the npm half of `scripts/bump-harness-versions.sh`. Node (an `ARG` Dependabot does not bump) and pi still need the script or a lock of their own |
| Watch | microsandbox, as one backend for Linux, macOS and Windows | The only candidate that could. With no NIC its network holds, but the image's root is a read-write mount, and its vsock route stalls on a request over 32 KB, so a real call never completes; open upstream as #1878, not fixed in 0.7.9 |
| Target | kubernetes-sigs/agent-sandbox for the institutional shape, Agent Substrate later | On kind, the egress as a sidecar and our backend inside the harness container, under a seccomp profile that allows `unshare`, passed every check; neither replaces the laptop launcher |

## The contract any replacement has to hold

From `crates/sandbox/src/lib.rs` and PLAN.md M5, M6 and M8:

1. **No network.** The harness has no interface but loopback. Its only way out is one unix
   socket to the egress, bind-mounted in and relayed by `amem relay` to a loopback port. The
   egress is the only process with the network.
2. **A fixed filesystem.** The system read-only, a private home and the working tree
   read-write, the memory binary and the record's projection read-only, nothing else of the
   host. On Linux at fixed paths: `/home/user`, `/opt/memory/memory`, `/run/memory/egress.sock`,
   `/memory`.
3. **Unprivileged.** No root, no daemon, no container runtime on Linux.
4. **macOS**: Seatbelt, dry-coded. **Windows**: WSL containers, the egress in one container and
   the harness in another with `--network none`, sharing the socket through a volume.
5. **One image** for the containers backend, `images/harness`, pinned.
6. **The egress**: model calls forwarded per destination with the credential injected, the
   label checked against the destination's ceiling first, token usage metered per session
   from every response including streamed ones, per-session and per-day token limits, a
   CONNECT tunnel to an allowlist for a harness with no base-URL override, OpenTelemetry
   traces, and `/mcp` on the same socket. Cost is priced outside the egress, by `amem cost`
   from `prices.toml`.

What is ours today, by size (`wc -l`, tests included): `crates/sandbox` 5,094 lines, of which
the Linux backend is 373 and Seatbelt 768, and the harness adapters, which stay whatever is
adopted, are most of `lib.rs`; the Windows path is `crates/wslc-api` (1,345) plus
`crates/memory/src/wslc.rs` and `wslc_api.rs` (1,873); `crates/egress` 3,372; the image is a
64-line Dockerfile and a 38-line bump script.

## What the spikes showed

One check, `contract-check.py`, run inside each box against the same fixture: a scratch tree
laid out as the launcher lays out a session, a stand-in egress on a unix socket, and a marker
file in a host home that must stay invisible. It asserts point 1 (only loopback, no TCP out,
no name resolution, no vsock, the egress answers) and point 2 (home and tree writable,
projection, system and memory binary read-only, no writable mount under the read-only system,
the marker invisible). Linux on WSL2, kernel 6.18.40, x86_64.

"Read-only" is the mount's flag (`statvfs`), and "invisible" is "no such file". The first
version tried a write instead, and took "exists" as false on any error. The pre-PR review
showed both passing on an unconfined host, where file mode alone refused the write. Every
row below is from the corrected check, run on 23 October.

| Box | Failed |
|---|---|
| host, unconfined (the control) | 9 of 12, as it must |
| **ours**, `amem launch --harness command` | no vsock; a writable mount under `/usr` |
| rootless podman, runc, `--network none` | no vsock |
| nsjail, built from master (`4ff54a6`), with a seccomp rule refusing vsock and io_uring | none |
| nsjail without the rule | no vsock |
| ferroday-cage `fcage` 0.5.0, with the same rule | none |
| fcage without the rule | no vsock |
| gVisor `release-20261012.0`, rootless, systrap | none |
| gVisor, the same, on its `kvm` platform | none |
| **ours**, Seatbelt, macOS 26.6 on GitHub's runner, with the fix below | none; the interfaces are reported, not checked, since Seatbelt denies their use rather than removing them |
| microsandbox 0.7.8, `msb run --net none` | only loopback: the guest keeps `eth0` and `dummy0`, denied by policy rather than absent; the system is a read-write mount |
| microsandbox 0.7.8, Rust SDK, `disable_network()` | only loopback, on the name alone: the guest has no network device, and `dummy0`, its kernel's discard device, is down with no address and no route; the system is a read-write mount |

The harness smoke test, `harness-smoke.sh`, runs Claude Code 2.1.296, Codex 0.162.1 and Gemini
CLI 0.63.0 from one Node rootfs: all three start under podman and under gVisor, and Claude
Code's calls reach the stand-in egress through a loopback relay under both. The stand-in,
outside the box, logged 7 requests under podman and 6 under gVisor in 20 seconds.

A real call, `harness-call.sh`: Claude Code asked for one word on a subscription login. The
stand-in egress forwarded to `api.anthropic.com` and logged each request. The box got only the
login's access token, never its refresh token, so nothing inside could rotate the host's login.

| Box | Result |
|---|---|
| **ours**, `amem launch --harness claude`, the real egress | answered; the egress metered 1 call, 2 tokens in, 4 out, 10,182 from cache |
| rootless podman, runc | answered: one `POST /v1/messages`, 200 |
| gVisor, systrap | answered: one `POST /v1/messages`, 200 |
| gVisor, kvm | answered: one `POST /v1/messages`, 200 |
| microsandbox, SDK, no NIC | timed out; nothing reached the egress |

The microsandbox failure is its vsock route, not the harness. Tested over raw vsock inside the
guest:
- Requests of up to 32 KB arrive whole and are answered at once.
- From 48 KB, the host gets the start of the request, then nothing.
- Sent in 8 KB writes, 128 KB arrives.
- 1 MB stops at 262,080 bytes.

That is upstream issue #1878, "vsock route (guest → host unix socket) stalls after 256 KiB of
upload when the host does not write back", open, and not addressed in 0.7.9. Claude Code's
first request carries some 10,000 tokens of prompt (the cache figure above), well over 32 KB,
and an egress that reads a request before answering never writes back early. The same binary in the same image answered under podman.

What gVisor costs, from `bench-compare.sh` on the same rootfs. Each figure is the median of
three interleaved rounds on a quiet machine, on 23 October. runc is in milliseconds, each
gVisor column a multiple of it:

| Task | runc | gVisor | mounts cached | kvm platform |
|---|---|---|---|---|
| Node start, 20 times | 560 ms | 3.1x | 3.2x | 4.1x |
| `claude --version`, 5 times | 60 ms | 3.6x | 3.8x | 4.8x |
| 10,000 small files written, read and removed in the working tree | 776 ms | 5.6x | 5.2x | 4.5x |
| the same in `/tmp` | 360 ms | 2.9x | 3.1x | 6.2x |
| copy a `node_modules` tree in `/tmp` | 118 ms | 2.2x | 2.1x | 2.2x |
| a CPU loop in Node | 1,056 ms | 1.2x | 1.2x | 1.2x |

The run on 20 October, without the kvm column, gave multiples within 0.6 of these.

Caching the mounts (`--file-access-mounts=exclusive`) changes nothing. It is safe only if
nothing on the host edits the tree during the session. So the cost is the syscall
interception of the `systrap` platform, not revalidation.

The `kvm` platform is no cheaper here: slower everywhere except the working tree. WSL2 is
itself a Hyper-V guest, so `kvm` runs nested here, which is the likely reason. On bare metal
it was not measured. What either cost adds to a real session, most of which is spent waiting
on the model, was not measured.

A second nested reading, from the workflow's `gvisor-kvm` job on GitHub's Ubuntu 26.04 runner
(an Azure VM, kernel 7.0.0-1017-azure, run `40019093054`), one round each. The contract check
passed every check on both platforms there. kvm is the slower again:

| Task | runc | gVisor | mounts cached | kvm platform |
|---|---|---|---|---|
| Node start, 20 times | 469 ms | 1.9x | 1.9x | 4.6x |
| `claude --version`, 5 times | 44 ms | 2.6x | 2.6x | 6.6x |
| 10,000 small files in the working tree | 321 ms | 4.3x | 3.7x | 8.4x |
| the same in `/tmp` | 309 ms | 2.1x | 2.1x | 6.1x |
| copy a `node_modules` tree in `/tmp` | 92 ms | 1.7x | 1.7x | 2.2x |
| a CPU loop in Node | 864 ms | 1.0x | 1.0x | 1.1x |

## Five gaps in our own code

**vsock.** A network namespace does not cover `AF_VSOCK`. Inside our sandbox a vsock socket
can be created, and on WSL2, or any VM guest with a vsock device, it addresses the hypervisor's
host whatever the namespace says. Every box built on namespaces failed this check until a
seccomp rule refused the family, and passed with it; gVisor passes because its netstack has no
vsock. Codex (`codex-rs/linux-sandbox/src/seccomp.rs`) and sandbox-runtime
(`vendor/seccomp-src/seccomp-unix-block.c`) refuse `AF_VSOCK` and `io_uring_*` for this reason.
`crates/sandbox` has no seccomp filter and does not set `PR_SET_NO_NEW_PRIVS`. Whether
anything on the WSL2 host answers was not established; that the route exists was.

**A read-only bind that is not recursive.** Our backend binds `/usr` with `MS_BIND | MS_REC`
and then remounts it read-only (`crates/sandbox/src/linux.rs:63-80`); the remount applies to
the top mount only. On WSL2, `/usr/lib/wsl/lib` and `/usr/lib/modules/<kernel>` are separate
mounts and stay read-write inside. A write there failed only because the files are root-owned
and show as `nobody` inside. Any host submount the user owns under a read-only grant would be
writable from the sandbox. podman, nsjail, fcage and gVisor did not show it. `mount_setattr(2)`
with `AT_RECURSIVE` and `MOUNT_ATTR_RDONLY` (Linux 5.12) is the fix.

**The CONNECT tunnel.** `connect_or_not_found` takes the port from the request's authority and
`TcpStream::connect` resolves and dials it (`crates/egress/src/lib.rs:756-771`). An allowed name
is reachable on any port, and a name that resolves to loopback, a private range or a metadata
address is dialled. sandbox-runtime, iron-proxy, Smokescreen, Codex's network proxy and
Envoy's `resolved_address_filter` all guard this. The fix: 443 unless the allowlist entry names
a port, resolve first, refuse a denied range, dial the checked address.

**A socket path past 108 bytes.** The egress socket lives under the root, at
`state/run/egress-<pid>.sock`. A unix socket path has a 108-byte limit. With a root whose
socket path came to 146 bytes, `amem launch` printed
`bind .../egress-<pid>.sock -> /tmp/memory-sandbox-<pid>/run/memory/egress.sock: ENOENT` and
then `spawn /opt/memory/memory: Invalid argument`, which name neither cause. The same launch
from a short root completed the real call above.

The cause is in the launcher. It spawns `serve_unix` as a task, waits up to two seconds for the
socket to appear, and goes on whether or not it did (`crates/memory/src/main.rs:3482-3494`).
The bind error stays in the task. The fix: check the path length before binding, or wait on the
bind's result rather than on the file.

**A Seatbelt profile under which nothing starts.** Our macOS backend was dry-coded, and this
was its first run on a Mac: GitHub's `macos-latest`, macOS 26.6.2.

What happened, run by `.github/workflows/harness-spikes.yml`:
- Every `amem launch` exited 1 without a word, `/usr/bin/true` included (`seatbelt-steps.sh`).
- Run under `sandbox-exec` directly with the profile's shape rebuilt (`seatbelt-bisect.sh`),
  `/usr/bin/true`, `amem --version` and the relay each died on SIGABRT.
- Under `(allow default)` they ran.

The cause, bisected:
- The only Sandbox reports naming these processes were `deny(1) file-read-data /` and
  `deny(1) file-write-data /dev/dtracehelper`.
- Adding `(allow file-read-data (literal "/"))` alone let all three start, and let a shell run
  through the relay. The `dtracehelper` grant changed nothing.
- The profile grants `/` its metadata only (`crates/sandbox/src/seatbelt.rs`, the metadata rule
  of `render`). The process start reads the root directory's listing, and aborts when refused.

Why nothing caught it: the launcher reports a death by signal as exit 1 without saying so,
in two places (`crates/sandbox/src/measure.rs:73`, the `wait` function, and the relay at
`crates/memory/src/main.rs:3986`). And the macOS CI job that runs this backend's
`sandbox-exec` tests failed to compile on the last two runs on `main` (`0eacdb8`, `3ed7b48`):
`fn io` in `crates/mount/src/lib.rs:57` is dead code on macOS, an error under `-D warnings`.
So those tests did not run.

**Fixed on this branch.** `609304d` adds the grant, with a unit test for the rule and a macOS
test that `/usr/bin/true` starts and a shell returns its own code. On `60677f6`, without the
grant, that test and the two older macOS tests failed in the macOS job (`left: 1, right: 0`).
With the grant they pass.

The contract check then ran under our backend for the first time. It passed every check but
the host marker: the sandbox refused the `stat` with EPERM, which on macOS is the
confinement, and the check now reads it so. `535e356` grants `/private/var/select` with the
system. Without it every shell printed `Error opening /private/var/select/sh`, and Python's
shim printed an `xcode-select` error.

Two things remain:
- **The developer-tools shims still warn.** `/usr/bin/python3` reads
  `/private/var/db/xcode_select_link` and the active Xcode under `/Applications`, whose path
  differs from one machine to the next. It prints an `xcode-select` warning and runs.
- **The CI macOS job still does not compile.** It is the `crates/mount` dead code, for a branch
  of its own. Until it compiles, the spikes workflow runs this backend's tests on a Mac, on
  a pull request that changes `crates/sandbox` and on one that changes the spikes.

## Embeddable confinement

What the harnesses ship, read from their trees: on Linux, Codex, sandbox-runtime (Claude
Code's), Gemini CLI and Copilot CLI all run bubblewrap, compiled in or exec'd, and keep host
paths; Cursor goes straight to Landlock and seccomp; on macOS everyone uses Seatbelt; on
Windows each has its own (sandbox users with WFP, restricted tokens, ProcessContainer). Nobody
links a permissive namespace library, and none gives fixed paths inside the box on Windows, so
WSL containers stay our only route to point 2 there.

| Candidate | Licence | Verdict | Why |
|---|---|---|---|
| `seccompiler` (rust-vmm) and `landlock` | Apache-2.0 OR BSD-3; MIT OR Apache-2.0 | **adopt** | Pure Rust; closes the vsock gap and adds Landlock under our mounts |
| Codex `codex-rs` sandbox crates | Apache-2.0 | **borrow** the seccomp rules and Seatbelt profiles | Not on crates.io (workspace version 0.0.0); Linux runs bubblewrap; host paths, so no point 2 |
| nsjail | Apache-2.0 | **fallback** | One binary, held both points here with the rule; the 3.6 release needs libnl-3 (LGPL), master does not |
| ferroday-cage | MIT OR Apache-2.0 | **watch** | The library shape we would want, held both points here; three months old, one maintainer |
| nono | Apache-2.0 | **borrow** ideas | Landlock-first, loopback-only network namespace with a supervisor-held listener, close to our relay; no mount namespace, so no point 2 |
| sandlock (multikernel) | Apache-2.0 | **borrow** | Landlock plus seccomp in Rust, a second layer inside our namespaces; read, not run |
| agent-safehouse | Apache-2.0 | **borrow** | Per-harness Seatbelt rules, a cross-check for our generated profile; read, not run |
| Microsoft MXC `mxc-sdk` | MIT | **revisit** for a native Windows backend | Linux runs bubblewrap; its WSL containers path has no stdin and no mount targets, so it does not replace `crates/wslc-api` |
| sandbox-runtime (`srt`) | Apache-2.0 | **learn from** | Same network design as ours; execs bubblewrap and socat (GPL) on Linux, reads the whole host by default |
| youki `libcontainer` | Apache-2.0 | **learn from** | Heavy; its `pathrs` dependency is MPL-2.0 or LGPL-3.0 |
| gaol, extrasafe, landrun, rappct, isobox | various permissive | reject | Stale, Landlock only, or too small to trust under a boundary |
| bubblewrap, firejail, hakoniwa, birdcage, syd, crun | LGPL, GPL | **out** on licence | |

## External runtimes

How the egress socket crosses the boundary decides most of this layer. Same kernel (runc,
podman, gVisor): bind the socket in, as now; gVisor needs `--host-uds=open`. A VM: a unix
socket on a shared filesystem cannot be connected from the guest, so the VMM maps a guest
vsock port to the host socket instead, and `amem relay` needs a vsock client (Apple's runtime
is the exception, presenting a unix socket in the guest). Or the egress moves into the VM
beside the harness, which is what our WSL containers backend does.

| Candidate | Licence | Verdict | Why |
|---|---|---|---|
| **gVisor** (`runsc`) | Apache-2.0 | **adopt** as an optional Linux backend | Passed every check, rootless, no KVM, no daemon, on both `systrap` and `kvm`; the three harnesses ran under it, and a real call completed. The box runs as uid 0 inside (the caller outside), because rootless `runsc` refuses a uid map of its own; binding the host's `/usr` fails rootless on WSL2, so the root comes from the image; about 100 MB of binaries to pin |
| **microsandbox** | Apache-2.0; its guest kernel, libkrunfw, is LGPL-2.1 and GPL-2.0 | **watch** | The only candidate for one backend on Linux, macOS and Windows from one image. With no NIC its network holds; its root is writable, and its vsock route cannot yet carry a real request. See below |
| Apple `container` / Containerization | Apache-2.0 | **spike on a Mac** | `--network none`, and a `-v` mounted host socket is relayed in as a unix socket, so the relay is unchanged; needs macOS 26 on Apple Silicon and an arm64 image (the Dockerfile hard-codes the x64 Node tarball). Read, not run |
| libkrun (direct) | Apache-2.0 | fallback for microsandbox | The engine under it; a binding crate and our own guest init |
| runc, podman | Apache-2.0 | **learn from** | Held both points here bar vsock; no isolation gain over our namespaces on Linux |
| Firecracker | Apache-2.0 | reject | No virtio-fs for a live working tree; the jailer needs root |
| Kata Containers | Apache-2.0 | reject | Shim and virtiofsd run as root |
| Lima, Colima, Cloud Hypervisor, crosvm, vfkit, smolvm, BoxLite, Gondolin | permissive | **learn from** | VM managers or building blocks, not a boundary we would use as is |
| Hyperlight | Apache-2.0 | reject | Cannot run a Linux userland |
| crun | GPL-2.0-or-later | **out** | |
| Docker Sandboxes (`sbx`) | proprietary | **out** | Its credential proxy matches our egress model |

**microsandbox, run here.** `msb run` booted the image as a libkrun microVM with the fixture
mounted over virtiofs at the Linux layout, as uid 1000, and the egress mapped from guest vsock
port 5000 to the host socket. Everything held except one thing: the CLI's `--net none` is a
deny-all policy, and the guest keeps `eth0` and `dummy0`, so TCP out is refused by the host's
policy rather than unreachable. That is a policed network, not point 1. The SDK's
`disable_network()` sets `network.enabled = false` (`sdk/rust/lib/sandbox/builder.rs:657`),
which the CLI cannot express.

A Rust probe that uses it, in `spikes/harness-containers/msb-probe/`, booted a guest with no
network device: its virtio devices are console, block, virtiofs, vsock, balloon and rng, and
no net. "Only loopback" failed on the name alone: `dummy0`, the guest kernel's own discard
device, down, with no address and no route.

The system is another matter, under the CLI and the SDK alike. The image's root is mounted
read-write. It is the sandbox's own copy-on-write layer, so the host is not exposed. But a
harness can change its own `/usr` for the sandbox's life, and point 2 says the system is
read-only. The first version of the check missed this: uid 1000 cannot write root-owned
files, and the check took that for a read-only mount. Neither the CLI's help nor the SDK's
builder offers a read-only root.

The vsock route answered on port 5000 and no other port on the host did. It does not carry a
large request: past 32 KB in one write, or 256 KB in all, the upload stalls until the host
writes something back (the real-call table above, and upstream #1878). Until that is fixed,
no harness completes a call through it. It is beta ("expect breaking changes"), keeps state in a SQLite database under
`MSB_HOME`, and derives a unix socket path from `MSB_HOME` that must stay under 108 bytes.

## The harness image

| Option | Licence | Verdict | Why |
|---|---|---|---|
| Keep the Dockerfile; add `package.json` and `package-lock.json` for the harnesses, `npm ci`; `FROM ubuntu:24.04@sha256:...`; Dependabot for npm, docker and github-actions | Dependabot MIT | **adopt** | Built: same size and build time as today, every package pinned with an integrity hash, and the base by digest. Retires the npm half of `bump-harness-versions.sh`; Node's version is an `ARG`, which Dependabot does not bump, so that half stays. pi needs a lock of its own, and Muse Code stays a script step |
| numtide/llm-agents.nix with nix2container | MIT; Apache-2.0 | **learn from** | Built: all nine harnesses, reproducible to the image id, Muse Code pinned by hash. But a third larger, ahead of our pins on eight of nine (taking numtide's versions, or overriding each), 281 of 288 store paths from numtide's cache, and Nix itself (LGPL-2.1) in the build |
| Docker's sandbox kit spec and kits | Apache-2.0 (the `sbx` tool is proprietary) | **borrow** | Per-harness credential and allowlist declarations; their harness versions float |
| apko and melange (Wolfi) | Apache-2.0 | learn from | Wolfi packages only Codex and Node of our set |
| Dev Container Features and CLI | MIT | reject | The lock pins the feature, not the tool it installs; Anthropic's feature has no version option; no WSL containers driver |
| Anthropic's Claude Code devcontainer | all rights reserved | out | Also keeps a network and needs `NET_ADMIN` |

All three were built and run through the contract check and the harness smoke under podman
on 20 October (`spikes/harness-containers/image-lockfile/`, `image-nix/`); each passed as the
podman baseline does, failing only the vsock check, which is the runtime's, not the image's:

| Image | Build | Size | Pinned | Versions |
|---|---|---|---|---|
| today's Dockerfile | 320 s | 3.15 GB | the 7 npm harnesses' own versions; base by tag | our pins |
| with the lockfile | 313 s | 3.15 GB | 107 lockfile packages, each with an integrity hash; base by digest | our pins |
| llm-agents.nix, nix2container | 360 s from an empty store, plus 116 s to load; 61 s unchanged | 4.08 GB | the flake lock, every store path by hash | numtide's, newer than ours on eight of nine |

The lockfile closes less than it looks: the seven npm harnesses install only 30 or 31
packages between them, most shipping as bundles or per-platform binaries, so most lock entries
are platform variants that never install. The large float is pi, 119 packages through
`npm -g`, which needs its own lock because it installs with `--ignore-scripts` while Claude
Code's postinstall has to run. Both npm builds report five install scripts "not covered by
allowScripts"; they ran under npm 11.19, and whether npm 12 blocks them is to check before
the next Node bump. The proposed `dependabot.yml` is in the lockfile spike directory, not yet
in `.github/`.

The repository has GitHub Actions workflows and no `.github/dependabot.yml`. The research
reports that ASF Infra's Dependabot page requires one for the github-actions ecosystem on every
repository that uses Actions; that page was read by the research agent, not checked again here.

## The egress, and who else meters tokens

The slices a third party could take: the CONNECT allowlist with an address guard, credential
injection, TLS termination so a tunnel can be metered, and metrics. What stays ours: the
socket-and-relay topology, the label check (it needs the session's read history), per-session
metering and limits, and `/mcp` served by `dm_server`.

Who meters tokens, from streamed responses included, read from each tree: agentgateway, Agent
Router (formerly Envoy AI Gateway), LiteLLM's MIT part, agentsh, Busbar, Bifrost, AISIX, Plano
and GoModel; ccusage reads it from harness logs afterwards. Every sandbox-side proxy
(sandbox-runtime, Codex's network proxy, iron-proxy, OpenShell, Gondolin, microsandbox, Docker
Sandboxes, Agent Substrate) injects credentials, holds an allowlist and has a hook before a
request leaves, and none counts tokens.

| Candidate | Licence | Tokens, streamed | Unit | Cost, budgets | Injection | Allowlist | Pre-egress hook | Metrics | MCP |
|---|---|---|---|---|---|---|---|---|---|
| **ours** | | yes, yes | session | priced outside; token limits per session and day | yes | CONNECT | label vs ceiling | traces only | `/mcp` |
| agentgateway | Apache-2.0 | yes, yes | API key, or a CEL key | price catalog; money and token budgets | yes | Substrate egress only | ext_authz, ext_proc, CEL | OTLP, Prometheus | gateway, per tool |
| Agent Router | Apache-2.0 | yes, yes | header | CEL cost; token rate limits | yes | no | Envoy filters | yes | `MCPRoute` |
| LiteLLM, MIT part | MIT outside `enterprise/` | yes, yes | key, user, team, session cap | yes; some budgets paid | yes | no | pre-call hook | yes | gateway |
| agentsh | Apache-2.0 | yes, yes | its session | token rate limits | yes | fail-closed | pre and post | yes | intercept |
| Busbar | Apache-2.0 | yes, yes | virtual key | rate card, budgets | yes | no | tap and gate | yes | no |
| sandbox-runtime | Apache-2.0 | no | command | no | sentinel swap | default deny | external decider | no | no |
| iron-proxy | Apache-2.0 | no | address | no | token swap | default deny, CIDR deny | gRPC transform | OTLP logs | tool allowlist |
| Smokescreen | MIT | no | mTLS role | global rate | static headers on intercepted domains | per role | post-decision handler | Prometheus | no |
| NVIDIA OpenShell | Apache-2.0 | no | sandbox | no | placeholder | default deny | supervisor middleware | yes | yes |
| Agent Substrate | Apache-2.0 | no | actor | no | `replace_headers` | `EgressPolicy` | replaceable PEP | partial | no |
| microsandbox | Apache-2.0 | no | sandbox (bytes) | bandwidth limits | placeholder secrets | rules | no | OTLP resource metrics | no |

Closest to ours is **agentgateway**: a Rust binary for all three platforms that reads input,
output and cache tokens from JSON and streamed Anthropic and OpenAI responses, prices them from
a catalog, enforces money or token budgets, injects credentials, exposes hooks, exports gen_ai
metrics, and ships a Substrate gateway mode. It has no unix-socket listener, no session unit
(a CEL key fed a session header would serve), no label rule (an ext_authz call back to us), and
it would front our `/mcp`, not serve it. It could take the model-forwarding leg behind our
egress, at the price of a second process holding the network and every call metered twice.
Worth it only if its money budgets and Prometheus metrics are wanted more than one process
fewer. Envoy itself is rejected: a large C++ daemon with a config language beside a Rust
binary whose whole job is this; its `resolved_address_filter` is the idea to take for the
CONNECT fix. Infisical Agent Vault (MIT) does placeholder-key injection on a CONNECT path,
which is the shape a metered Copilot tunnel would need.

## Whole platforms

One fact shapes this layer: every container in a Kubernetes pod shares one network namespace,
so a pod by itself cannot give "harness offline, egress sidecar online". The institutional
shape holds point 1 only if our Linux backend runs inside the harness container (which needs a
seccomp profile that allows `unshare`, `hostUsers: false`, or Kata), or if the egress runs in
its own pod and the harness pod reaches it over TCP behind a deny-all policy.

That was settled on kind (rootless podman, Kubernetes 1.37, agent-sandbox 1.0.7), with the
contract check run in the harness container
([spikes/harness-containers/agent-sandbox](../spikes/harness-containers/agent-sandbox/)):

| Layout | Result |
|---|---|
| A: harness and egress sidecar in one pod | point 1 fails: the harness reaches the API server, cluster DNS, other pods and the internet |
| B: the same pod, our namespaces backend inside the harness container, default seccomp | `unshare` refused |
| B, seccomp Unconfined | the network is cut, but vsock is open, as in our own backend |
| **B, a Localhost seccomp profile: the runtime default plus `unshare`, `setns`, `clone` and mounting, still refusing `AF_VSOCK`** | **every check passes**, and from a `SandboxTemplate` through a warm pool and a claim too |
| B, `hostUsers: false` | the pod does not start: on a rootless node for two reasons, and on a rootful one because `runc` may not mount `sysfs` |
| C: the egress in its own pod, the harness pod deny-all but to it | the policy is enforced (kindnet runs kube-network-policies), but the route out is pod-network TCP, not a socket, so point 1 holds only as far as the network plugin does, and the egress Service is reachable from any pod a policy does not stop |

So the institutional shape is B: the egress as a native sidecar with the socket in a shared
`emptyDir`, our Linux backend inside the harness container, and a seccomp profile that every
node carries, which is a cluster administrator's step (the Security Profiles Operator, for
one). kind itself needed no root here, only podman's log driver set to `k8s-file`.

`hostUsers: false` met two walls on the rootless node.

**The kubelet's id ranges.** With no `kubelet` entry in the node's `/etc/subuid`, the kubelet
picks each pod's 65,536 ids at random from the whole 32-bit range. It gave one pod
2,558,984,192 and another 1,137,049,600. The node maps only the ids its user was given, and
16 million more on the host changed nothing. containerd's `setresuid` to such an id fails
with `invalid argument` (`pkg/sys/unshare_linux.go:167`). Giving the kubelet a range inside
the node's map fixed it: a `kubelet` user, `getsubids` installed, and `kubelet:131072:7208960`
(110 pods of 65,536).

**Idmapped mounts.** containerd then builds the pod's root with an idmapped overlay, and
`mount_setattr` refused with `operation not permitted`. The node's `/var` is a podman volume
on the host's ext4. The rootless node holds no privilege over that filesystem, which an
idmapped mount needs.

So `hostUsers: false` needs a rootful node. **On one, it still did not start.** The workflow's
kind job ran the node under Docker on GitHub's Ubuntu 26.04 runner (run `40019093054`); the
node's uid map was the whole range, so it was rootful. Both `hostUsers: false` pods failed to
create their sandbox:

```text
runc create failed: unable to start container process: error during container init:
error mounting "sysfs" to rootfs at "/sys": ... flags=MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC:
operation not permitted
```

The kernel lets a user namespace mount `sysfs` only when the namespace owns the network
namespace and the mounter's own `/sys` is not partly covered; which of the two failed is not
known yet. The runner now prints the node's kernel, containerd, runc and `/sys` mounts when
the pod fails. The same run's other layouts did not start either: the image did not load into
the node, since Docker's store saved an index naming platforms it held no layers for. The
runner saves the one platform now. Whether `hostUsers: false` alone admits `unshare` under
the default profile is still unseen.

| Candidate | Licence | Verdict | Why |
|---|---|---|---|
| kubernetes-sigs/agent-sandbox | Apache-2.0 | **target** for the institutional shape | A full PodSpec per sandbox, warm pools, claims, expiry; ships a recall-memory example close to our design |
| Agent Substrate | Apache-2.0 | **target** later, learn from now | Agents in gVisor or Cloud Hypervisor on Kubernetes; all egress through one replaceable gateway with an allowlist and credential injection, which our egress could be. Pre-1.0, needs a cluster, PostgreSQL and an object store; no host bind mounts, so no point 2 on a laptop; Linux nodes only |
| OpenSandbox | Apache-2.0 | **borrow** | The closest open design: a per-sandbox egress sidecar holding credentials, Docker or Podman to Kubernetes; its egress is a transparent intercept, not a socket |
| NVIDIA OpenShell | Apache-2.0 | **learn from** | Our architecture: no network in the box, one socket to a supervisor, injection, a hook, MCP checks, OTel; needs a gateway daemon and Podman 5 or KVM |
| E2B (SDK and runtime) | Apache-2.0 | learn from | Privileged Docker stack, KVM, about 12 GiB; no host socket or bind mounts |
| Dagger, container-use | Apache-2.0 | reject | No network-off; container-use leaves the agent on the host |
| OpenHands, SWE-ReX | MIT | reject | Need a TCP port into the box |
| Daytona, Coder, Arrakis | AGPL-3.0 | **out** | Daytona's core repository is archived, development moved private |
| DevPod | MPL-2.0 | out (Category B) | |
| Docker Sandboxes | proprietary | **out** | |
| Hosted services (Cloudflare, Vercel, Modal, Runloop, Blaxel, Morph, Fly Sprites, Together) | open SDKs only | out of scope | A laptop-first, self-hosted reference cannot depend on them |

"Substrate" means Agent Substrate here. The other projects of that name are unlicensed,
hosted, orchestration above the harness, or the Polkadot framework.

## What was not done

- Apple `container`, and anything else that needs a Mac's virtualization.
- gVisor's `kvm` platform on bare metal; here it ran nested under Hyper-V.
- agent-sandbox with `hostUsers: false` past the `sysfs` refusal, and every other layout on
  the rootful node: the next run of the workflow's kind job, with the image loading.
- A real call through nsjail and fcage. Their builds went with the scratch directory, and they
  bind the socket in exactly as podman does, where the call completed.
- microsandbox again once #1878 is fixed: `harness-call.sh` through `run-microsandbox-sdk.sh`
  is the test.
