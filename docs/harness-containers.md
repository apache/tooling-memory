# Harness containers: what exists that we could use instead of our own

We wrote this report on 20 October 2026. On 23 October we added the runs that needed `/dev/kvm`,
a larger uid range and a real login. We built these parts to confine a harness and to route its
calls: `crates/sandbox`, the WSL containers backend, `images/harness` and `crates/egress`. The
question is which of these parts existing work can replace, and which parts should stay ours.
The existing work must have the Apache-2.0 licence or a more permissive licence.

We surveyed five layers:
- embeddable confinement;
- external runtimes;
- the harness image;
- the egress beside the sandbox;
- whole agent-sandbox platforms.

We read each licence below from the repository or the crate manifest on 19 or 20 October 2026,
never from memory. We used `gh api repos/OWNER/REPO/license`, and the LICENSE file where that
command said NOASSERTION. We ran the top candidates on one machine against one check.
[spikes/harness-containers](../spikes/harness-containers/) holds the scripts and the raw
results. Where we read a candidate but did not run it, the text says so.

**Allowed**: Apache-2.0, MIT, BSD, ISC, Zlib, BSL-1.0, CC0, the Foundation's Category A.
**Noted, ranked as out**: MPL-2.0 and EPL (Category B). **Out**: GPL, LGPL, AGPL, SSPL, BUSL,
proprietary, and open-core when the part that we would need is the paid part. To execute an
LGPL binary that is installed separately is not to link it. But it is still a dependency that
we would ship or require. Thus bubblewrap is out.

## The answer, short

No project replaces the launcher and its egress. No project that we found does in one place all
that `crates/egress` does:
- one unix socket is the only way out of the sandbox;
- it meters tokens for each session from streamed responses;
- it checks the label against the ceiling before anything goes out;
- the memory server is on the same socket, over MCP.

Three things are worth taking, to use around it. We also found five gaps in our own code during
the research. By 25 October, `main` had fixes for all five. Each row names the pull request:

| Action | What | Why |
|---|---|---|
| Fixed, #14 | In our Linux backend, refuse `AF_VSOCK` and `io_uring` with seccomp, and set `no_new_privs` | The network namespace does not cover vsock. On WSL2 and on any VM guest, vsock reaches the host. We measured this (below) |
| Fixed, #15 | Make the read-only system bind recursive (`mount_setattr` with `AT_RECURSIVE`) | Submounts under `/usr` stayed writable inside our sandbox. Only file ownership prevented writes to them. We measured this (below) |
| Fixed, #13 and #24 | Restrict the ports of the CONNECT tunnel, and check the resolved address before dialling (#13). Check the TLS server name in the tunnel against the CONNECT host (#24) | The tunnel dialled any port on an allowed name, to any address that the name resolved to. The harness chose the server name after the 200 |
| Fixed, #12 | Add `.github/dependabot.yml`, for the actions of the workflows | No such file existed. npm and the base of the harness image wait for the lockfile row below |
| Fixed here, and #5, #17 | The Seatbelt profile now lists the root directory and reads the system's tool selections (`609304d`, `535e356`). The macOS CI job compiles (#5). The launcher reports a death by signal as a death by signal (#17) | On macOS 26.6, every process under the old profile aborted at start, `/usr/bin/true` too. With the fix, the contract check passes every check under Seatbelt on GitHub's runner (below) |
| Fixed, #16 | Fail the launch when the egress socket cannot be bound, and say why | With a root whose socket path was longer than 108 bytes, the launch continued. It failed later on a bind mount. We measured this (below) |
| Adopt | `seccompiler` (Apache-2.0 OR BSD-3) and `landlock` (MIT OR Apache-2.0) in `crates/sandbox`, with Codex's seccomp rule set as the reference | These libraries give the first fix, from the rule set that three harnesses already use. `seccompiler` is in, with #14. Landlock is not in yet |
| Adopt | gVisor as an optional hardened Linux backend | It passed every check here. It is rootless, and needs no KVM and no daemon. The Node harnesses run under it. A real Claude Code call completed through it |
| Adopt, lockfile done | An npm lockfile and a digest-pinned base for `images/harness`, bumped by Dependabot | We built it here, and it did not increase the size or the build time. It pins the packages that the harnesses pull in, and it pins the base. It retires the npm half of `scripts/bump-harness-versions.sh`. Node (an `ARG` that Dependabot does not bump) and pi still need the script or a lock of their own. The harness image now installs the seven npm harnesses from `images/harness/package-lock.json`. Thus an override can also hold `sharp` at a fixed version under qwen-code. The npm half of the script is not retired yet. It bumps `package.json` and the lock together, until Dependabot for npm does that work. The digest-pinned base and Dependabot for npm and docker are still open |
| Watch | microsandbox, as one backend for Linux, macOS and Windows | It is the only candidate that could do this. With no NIC, its network holds. But the root of the image is a read-write mount. Its vsock route stalls on a request over 32 KB, so a real call never completes. This is open upstream as #1878, and 0.7.9 does not fix it |
| Target | kubernetes-sigs/agent-sandbox for the institutional shape, Agent Substrate later | On kind, one layout passed every check. In it, the egress is a sidecar, and our backend is inside the harness container, under a seccomp profile that allows `unshare`. Neither replaces the launcher on a laptop |

## The contract any replacement has to hold

The contract comes from `crates/sandbox/src/lib.rs` and from PLAN.md M5, M6 and M8:

1. **No network.** The harness has no interface but loopback. Its only way out is one unix
   socket to the egress. The socket comes in as a bind mount, and `amem relay` relays it to a
   loopback port. The egress is the only process with the network.
2. **A fixed filesystem.** The system is read-only. A private home and the working tree are
   read-write. The `amem` binary and the projection are read-only. The sandbox shows nothing
   else of the host. On Linux, these are at fixed paths: `/home/user`, `/opt/memory/memory`,
   `/run/memory/egress.sock`, `/memory`.
3. **Unprivileged.** No root, no daemon, and no container runtime on Linux.
4. **macOS**: Seatbelt, dry-coded. **Windows**: WSL containers. The egress is in one container.
   The harness is in another container, with `--network none`. The two containers share the
   socket through a volume.
5. **One image** for the containers backend: `images/harness`, pinned.
6. **The egress**:
   - It forwards model calls for each destination, and injects the credential.
   - It checks the label against the ceiling of the destination first.
   - It meters token usage for each session, from every response, streamed responses too.
   - It enforces token limits for each session and for each day.
   - It opens a CONNECT tunnel to an allowlist, for a harness that has no base-URL override.
   - It sends OpenTelemetry traces.
   - It serves `/mcp` on the same socket.

   `amem cost` prices the cost outside the egress, from `prices.toml`.

Our code today, by size (`wc -l`, with tests):
- `crates/sandbox`: 5,094 lines. Of these, the Linux backend is 373 and Seatbelt is 768. The
  harness adapters are most of `lib.rs`, and they stay, whatever we adopt.
- The Windows path: `crates/wslc-api` (1,345), plus `crates/memory/src/wslc.rs` and
  `wslc_api.rs` (1,873).
- `crates/egress`: 3,372.
- The harness image: a 64-line Dockerfile and a 38-line bump script.

## What the spikes showed

We ran one check, `contract-check.py`, inside each sandbox against the same fixture. The fixture
has:
- a scratch tree, with the layout that the launcher gives a session;
- a stand-in egress on a unix socket;
- a marker file in a host home, which must stay invisible.

The check asserts point 1:
- only loopback;
- no TCP out;
- no name resolution;
- no vsock;
- the egress answers.

It asserts point 2:
- the home and the working tree are writable;
- the projection, the system and the `amem` binary are read-only;
- no writable mount is under the read-only system;
- the marker is invisible.

The host is Linux on WSL2, kernel 6.18.40, x86_64.

"Read-only" means the flag of the mount (`statvfs`). "Invisible" means "no such file". The
first version of the check tried a write instead. It also took "exists" as false on any error.
The pre-PR review showed that both checks passed on an unconfined host, where only the file
mode refused the write. Each row below comes from the corrected check, which we ran on 23
October.

| Sandbox, or the host | Failed |
|---|---|
| host, unconfined (the control) | 9 of 12, as it must |
| **ours**, `amem launch --harness command`, before #14 and #15 | no vsock; a writable mount under `/usr` |
| rootless podman, runc, `--network none` | no vsock |
| nsjail, built from master (`4ff54a6`), with a seccomp rule refusing vsock and io_uring | none |
| nsjail without the rule | no vsock |
| ferroday-cage `fcage` 0.5.0, with the same rule | none |
| fcage without the rule | no vsock |
| gVisor `release-20261012.0`, rootless, systrap | none |
| gVisor, the same, on its `kvm` platform | none |
| **ours**, Seatbelt, macOS 26.6 on GitHub's runner, with the fix below | none. The check reports the interfaces but does not check them, because Seatbelt denies their use and does not remove them |
| microsandbox 0.7.8, `msb run --net none` | only loopback: the guest keeps `eth0` and `dummy0`. A policy denies them, but they are present. The system is a read-write mount |
| microsandbox 0.7.8, Rust SDK, `disable_network()` | only loopback, only because of a name: the guest has no network device. `dummy0`, the discard device of its kernel, is down, with no address and no route. The system is a read-write mount |

The harness smoke test, `harness-smoke.sh`, runs Claude Code 2.1.296, Codex 0.162.1 and Gemini
CLI 0.63.0 from one Node rootfs. All three start under podman and under gVisor. Under both, the
calls of Claude Code go through a loopback relay to the stand-in egress. The stand-in is outside
the sandbox. In 20 seconds, it logged 7 requests under podman and 6 under gVisor.

A real call, `harness-call.sh`: Claude Code asked for one word, with a subscription login. The
stand-in egress forwarded to `api.anthropic.com` and logged each request. The sandbox got only
the access token of the login, never its refresh token. Thus nothing in the sandbox could rotate
the login of the host.

| Sandbox | Result |
|---|---|
| **ours**, `amem launch --harness claude`, the real egress | answered; the egress metered 1 call, 2 tokens in, 4 out, 10,182 from cache |
| rootless podman, runc | answered: one `POST /v1/messages`, 200 |
| gVisor, systrap | answered: one `POST /v1/messages`, 200 |
| gVisor, kvm | answered: one `POST /v1/messages`, 200 |
| microsandbox, SDK, no NIC | timed out; nothing reached the egress |

The cause of the microsandbox failure is its vsock route, not the harness. We tested over raw
vsock in the guest:
- Requests of up to 32 KB arrive complete and get an answer immediately.
- From 48 KB, the host gets the start of the request, then nothing.
- In 8 KB writes, 128 KB arrives.
- 1 MB stops at 262,080 bytes.

This is upstream issue #1878, "vsock route (guest → host unix socket) stalls after 256 KiB of
upload when the host does not write back". It is open, and 0.7.9 does not address it. The first
request of Claude Code carries approximately 10,000 tokens of prompt (the cache figure above).
That is much more than 32 KB. An egress that reads a request before it answers never writes
back early. The same binary in the same image answered under podman.

The cost of gVisor comes from `bench-compare.sh` on the same rootfs. Each figure is the median
of three interleaved rounds on a quiet machine, on 23 October. The runc column is in
milliseconds. Each gVisor column is a multiple of runc:

| Task | runc | gVisor | mounts cached | kvm platform |
|---|---|---|---|---|
| Node start, 20 times | 560 ms | 3.1x | 3.2x | 4.1x |
| `claude --version`, 5 times | 60 ms | 3.6x | 3.8x | 4.8x |
| 10,000 small files written, read and removed in the working tree | 776 ms | 5.6x | 5.2x | 4.5x |
| the same in `/tmp` | 360 ms | 2.9x | 3.1x | 6.2x |
| copy a `node_modules` tree in `/tmp` | 118 ms | 2.2x | 2.1x | 2.2x |
| a CPU loop in Node | 1,056 ms | 1.2x | 1.2x | 1.2x |

The run on 20 October had no kvm column. Its multiples were within 0.6 of these.

A cache of the mounts (`--file-access-mounts=exclusive`) changes nothing. The cache is safe
only if nothing on the host edits the working tree during the session. Thus the cost comes from
the syscall interception of the `systrap` platform, not from revalidation.

The `kvm` platform does not cost less here. It is slower in each task except the working tree.
WSL2 is itself a Hyper-V guest, so `kvm` runs nested here. This is the probable reason. We did
not measure it on bare metal. We did not measure what either cost adds to a real session. A
real session spends most of its time waiting for the model.

A second nested reading came from the `gvisor-kvm` job of the workflow, on GitHub's Ubuntu
26.04 runner (an Azure VM, kernel 7.0.0-1017-azure, run `40019093054`). Each figure is the
median of three interleaved rounds, as above. There, the contract check passed every check on
both platforms. Again, kvm is the slower platform:

| Task | runc | gVisor | mounts cached | kvm platform |
|---|---|---|---|---|
| Node start, 20 times | 469 ms | 1.9x | 1.9x | 4.6x |
| `claude --version`, 5 times | 44 ms | 2.6x | 2.6x | 6.6x |
| 10,000 small files in the working tree | 321 ms | 4.3x | 3.7x | 8.4x |
| the same in `/tmp` | 309 ms | 2.1x | 2.1x | 6.1x |
| copy a `node_modules` tree in `/tmp` | 92 ms | 1.7x | 1.7x | 2.2x |
| a CPU loop in Node | 864 ms | 1.0x | 1.0x | 1.1x |

## Five gaps in our own code

**vsock.** A network namespace does not cover `AF_VSOCK`. In our sandbox, a process can create a
vsock socket. On WSL2, or on any VM guest with a vsock device, that socket addresses the host of
the hypervisor. The namespace does not change this. Each sandbox built on namespaces failed this
check until a seccomp rule refused the family. With the rule, each passed. gVisor passes
because its netstack has no vsock. Codex (`codex-rs/linux-sandbox/src/seccomp.rs`) and
sandbox-runtime (`vendor/seccomp-src/seccomp-unix-block.c`) refuse `AF_VSOCK` and `io_uring_*`
for this reason. `crates/sandbox` had no seccomp filter and did not set `PR_SET_NO_NEW_PRIVS`.
We did not find out whether anything on the WSL2 host answers. We did find that the route
exists.

**Fixed in #14.** The child sets `no_new_privs` and installs a `seccompiler` filter as its last
step before exec. The filter refuses `AF_VSOCK` sockets and the `io_uring` calls. The child also
marks each inherited descriptor from 3 up as close-on-exec. Thus a vsock socket that the
launcher held cannot pass in.

**A read-only bind that is not recursive.** Our backend binds `/usr` with `MS_BIND | MS_REC`,
and then remounts it read-only (`crates/sandbox/src/linux.rs:63-80`). The remount applies only
to the top mount. On WSL2, `/usr/lib/wsl/lib` and `/usr/lib/modules/<kernel>` are separate
mounts. In the sandbox, they stay read-write. A write there failed only because root owns the
files, and the sandbox shows them as `nobody`. From the sandbox, a process could write to any
host submount that the user owns under a read-only grant. podman, nsjail, fcage and gVisor did
not show this gap. `mount_setattr(2)` with `AT_RECURSIVE` and `MOUNT_ATTR_RDONLY` (Linux 5.12)
is the fix.

**Fixed in #15.** Each read-only grant now gets `mount_setattr` with `AT_RECURSIVE`. On a host
without the call, a confined launch fails. It does not fall back to a remount for each mount.

**The CONNECT tunnel.** `connect_or_not_found` takes the port from the authority of the
request. Then `TcpStream::connect` resolves and dials it (`crates/egress/src/lib.rs:756-771`).
An allowed name is reachable on any port. The egress also dials a name that resolves to
loopback, to a private range or to a metadata address. sandbox-runtime, iron-proxy,
Smokescreen, Codex's network proxy and Envoy's `resolved_address_filter` all guard this. The fix
has four steps:
1. Port 443, unless the allowlist entry names a port.
2. Resolve first.
3. Refuse a denied range.
4. Dial the checked address.

**Fixed in #13**, as above. Then #24 fixed the next gap. An adversarial pass on #20 found it.
After the 200, the harness chose the TLS server name. Thus one allowed name on a shared front
end gave access to the other tenants of that front end. The egress now reads the ClientHello
before it dials. It refuses a ClientHello whose server name is not the CONNECT host. It also
refuses a ClientHello that carries Encrypted Client Hello. Two things that the egress cannot see
are still open: the HTTP `Host`, and the paths behind an allowed name.

**A socket path past 108 bytes.** The egress socket is under the root, at
`state/run/egress-<pid>.sock`. A unix socket path has a 108-byte limit. We used a root that
gave a socket path of 146 bytes. `amem launch` printed
`bind .../egress-<pid>.sock -> /tmp/memory-sandbox-<pid>/run/memory/egress.sock: ENOENT`. Then
it printed `spawn /opt/memory/memory: Invalid argument`. Neither message names the cause. The
same launch from a short root completed the real call above.

The cause is in the launcher. It spawns `serve_unix` as a task. It waits up to two seconds for
the socket to appear. Then it continues, whether the socket appeared or not
(`crates/memory/src/main.rs:3482-3494`). The bind error stays in the task. The fix: check the
path length before the bind, or wait for the result of the bind and not for the file.

**Fixed in #16.** The launcher binds before it starts to serve. If the path is too long, the
launch fails and gives the path and its length.

**A Seatbelt profile under which nothing starts.** Our macOS backend was dry-coded. This was its
first run on a Mac: GitHub's `macos-latest`, macOS 26.6.2.

`.github/workflows/harness-spikes.yml` ran these steps, with these results:
- Each `amem launch` exited 1 with no message. This included the launch of `/usr/bin/true`
  (`seatbelt-steps.sh`).
- We ran `/usr/bin/true`, `amem --version` and the relay directly under `sandbox-exec`, with
  the shape of the profile rebuilt (`seatbelt-bisect.sh`). Each died on SIGABRT.
- Under `(allow default)` they ran.

We bisected the cause:
- The only Sandbox reports that named these processes were `deny(1) file-read-data /` and
  `deny(1) file-write-data /dev/dtracehelper`.
- When we added only `(allow file-read-data (literal "/"))`, all three started, and a shell ran
  through the relay. The `dtracehelper` grant changed nothing.
- The profile grants only the metadata of `/` (`crates/sandbox/src/seatbelt.rs`, the metadata
  rule of `render`). At process start, the process reads the listing of the root directory.
  When the profile refuses this, the process aborts.

Why nothing found it: the launcher reports a death by signal as exit 1, and does not say so. It
does this in two places (`crates/sandbox/src/measure.rs:73`, the `wait` function, and the relay
at `crates/memory/src/main.rs:3986`). Also, the macOS CI job runs the `sandbox-exec` tests of
this backend. That job failed to compile on the last two runs on `main` (`0eacdb8`, `3ed7b48`).
`fn io` in `crates/mount/src/lib.rs:57` is dead code on macOS, which is an error under
`-D warnings`. Thus those tests did not run.

**Fixed on this branch.** `609304d` adds the grant. It also adds a unit test for the rule, and a
macOS test. The macOS test checks that `/usr/bin/true` starts and that a shell returns its own
code. On `60677f6`, without the grant, that test and the two older macOS tests failed in the
macOS job (`left: 1, right: 0`). With the grant they pass.

Then the contract check ran under our backend for the first time. It passed every check except
the host marker. The sandbox refused the `stat` with EPERM. On macOS, this refusal is the
confinement, and the check now reads it that way. `535e356` grants `/private/var/select` with
the system. Without it, each shell printed `Error opening /private/var/select/sh`, and the shim
of Python printed an `xcode-select` error.

Since then:
- **The CI macOS job compiles** (#5). Thus the `sandbox-exec` tests of this backend run on each
  pull request that is not docs only.
- **The launcher reports a death by signal** (#17). It exits 128 plus the number of the signal,
  as a shell does. It names the signal on one line.

One thing remains: **the developer-tools shims still warn.** `/usr/bin/python3` reads
`/private/var/db/xcode_select_link` and the active Xcode under `/Applications`. The path of the
active Xcode is different on each machine. `/usr/bin/python3` prints an `xcode-select` warning
and runs.

## Embeddable confinement

We read the source trees of the harnesses to find what they ship:
- On Linux, Codex, sandbox-runtime (the one in Claude Code), Gemini CLI and Copilot CLI all run
  bubblewrap, compiled in or exec'd. They keep host paths. Cursor uses Landlock and seccomp
  directly.
- On macOS, each harness uses Seatbelt.
- On Windows, each harness has its own method (sandbox users with WFP, restricted tokens,
  ProcessContainer).

No harness links a permissive namespace library. On Windows, no harness gives fixed paths inside
the sandbox. Thus on Windows, WSL containers stay our only way to hold point 2.

| Candidate | Licence | Verdict | Why |
|---|---|---|---|
| `seccompiler` (rust-vmm) and `landlock` | Apache-2.0 OR BSD-3; MIT OR Apache-2.0 | **adopt** | Pure Rust. It fixes the vsock gap and adds Landlock under our mounts. `seccompiler` is in since #14. Landlock is not in yet |
| Codex `codex-rs` sandbox crates | Apache-2.0 | **borrow** the seccomp rules and Seatbelt profiles | Not on crates.io (workspace version 0.0.0). On Linux it runs bubblewrap. It keeps host paths, so no point 2 |
| nsjail | Apache-2.0 | **fallback** | One binary. With the rule, it held both points here. The 3.6 release needs libnl-3 (LGPL); master does not |
| ferroday-cage | MIT OR Apache-2.0 | **watch** | It has the library shape that we would want, and it held both points here. It is three months old and has one maintainer |
| nono | Apache-2.0 | **borrow** ideas | Landlock first. A network namespace with only loopback, and a listener that a supervisor holds, similar to our relay. No mount namespace, so no point 2 |
| sandlock (multikernel) | Apache-2.0 | **borrow** | Landlock and seccomp in Rust, as a second layer inside our namespaces. We read it but did not run it |
| agent-safehouse | Apache-2.0 | **borrow** | Seatbelt rules for each harness, to cross-check our generated profile. We read it but did not run it |
| Microsoft MXC `mxc-sdk` | MIT | **revisit** for a native Windows backend | On Linux it runs bubblewrap. Its WSL containers path has no stdin and no mount targets. Thus it does not replace `crates/wslc-api` |
| sandbox-runtime (`srt`) | Apache-2.0 | **learn from** | The same network design as ours. On Linux it execs bubblewrap and socat (GPL). By default it reads the whole host |
| youki `libcontainer` | Apache-2.0 | **learn from** | Heavy. Its `pathrs` dependency is MPL-2.0 or LGPL-3.0 |
| gaol, extrasafe, landrun, rappct, isobox | various permissive | reject | Stale, Landlock only, or too small to trust under a boundary |
| bubblewrap, firejail, hakoniwa, birdcage, syd, crun | LGPL, GPL | **out** on licence | |

## External runtimes

How the egress socket goes through the boundary decides most of this layer:
- Same kernel (runc, podman, gVisor): bind the socket in, as now. gVisor needs
  `--host-uds=open`.
- A VM: the guest cannot connect to a unix socket on a shared filesystem. So the VMM maps a
  guest vsock port to the host socket instead, and `amem relay` needs a vsock client. Apple's
  runtime is the exception: it presents a unix socket in the guest.
- Or the egress moves into the VM beside the harness. Our WSL containers backend does this.

| Candidate | Licence | Verdict | Why |
|---|---|---|---|
| **gVisor** (`runsc`) | Apache-2.0 | **adopt** as an optional Linux backend | It passed every check, rootless, needs no KVM and no daemon, on both `systrap` and `kvm`. The three harnesses ran under it, and a real call completed. The sandbox runs as uid 0 inside (the caller outside), because rootless `runsc` refuses a uid map of its own. A rootless bind of the host's `/usr` fails on WSL2, so the root comes from the image. It has about 100 MB of binaries to pin |
| **microsandbox** | Apache-2.0; its guest kernel, libkrunfw, is LGPL-2.1 and GPL-2.0 | **watch** | The only candidate for one backend on Linux, macOS and Windows from one image. With no NIC, its network holds. Its root is writable, and its vsock route cannot yet carry a real request. See below |
| Apple `container` / Containerization | Apache-2.0 | **spike on a Mac** | It has `--network none`. It relays a host socket mounted with `-v` in as a unix socket, so the relay does not change. It needs macOS 26 on Apple Silicon and an arm64 image (the Dockerfile hard-codes the x64 Node tarball). We read it but did not run it |
| libkrun (direct) | Apache-2.0 | fallback for microsandbox | The engine under microsandbox; a binding crate and our own guest init |
| runc, podman | Apache-2.0 | **learn from** | They held both points here, except vsock. On Linux, they give no more isolation than our namespaces |
| Firecracker | Apache-2.0 | reject | No virtio-fs for a live working tree. The jailer needs root |
| Kata Containers | Apache-2.0 | reject | Shim and virtiofsd run as root |
| Lima, Colima, Cloud Hypervisor, crosvm, vfkit, smolvm, BoxLite, Gondolin | permissive | **learn from** | VM managers or components, not a boundary that we would use without change |
| Hyperlight | Apache-2.0 | reject | It cannot run a Linux userland |
| crun | GPL-2.0-or-later | **out** | |
| Docker Sandboxes (`sbx`) | proprietary | **out** | Its credential proxy matches our egress model |

**microsandbox, run here.** `msb run` booted the image as a libkrun microVM, as uid 1000. It
 mounted the fixture over virtiofs at the Linux layout. It mapped the egress from guest vsock
 port 5000 to the host socket. Everything held except one thing on the network. The `--net none`
 of the CLI is a deny-all policy, and the guest keeps `eth0` and `dummy0`. Thus TCP out is not
 unreachable: the policy of the host refuses it. That is a policed network, not point 1. The
 SDK's `disable_network()` sets `network.enabled = false`
 (`sdk/rust/lib/sandbox/builder.rs:657`). The CLI cannot express this.

A Rust probe in `spikes/harness-containers/msb-probe/` uses it. The probe booted a guest with no
network device. The virtio devices of the guest are console, block, virtiofs, vsock, balloon and
rng, and no net. "Only loopback" failed only because of a name: `dummy0`, the discard device of
the guest kernel. It is down, with no address and no route.

The system is a different problem, under the CLI and under the SDK. microsandbox mounts the root
of the image read-write. That root is the sandbox's own copy-on-write layer, so the host is not
exposed. But a harness can change its own `/usr` for the life of the sandbox. Point 2 says that
the system is read-only. The first version of the check did not find this. uid 1000 cannot
write files that root owns, and the check took that as a read-only mount. Neither the help of
the CLI nor the builder of the SDK offers a read-only root.

The vsock route answered on port 5000. No other port on the host answered. The route does not
carry a large request. Past 32 KB in one write, or past 256 KB in total, the upload stalls until
the host writes something back (the real-call table above, and upstream #1878). Until there is a
fix, no harness completes a call through it. microsandbox is beta ("expect breaking changes").
It keeps state in a SQLite database under `MSB_HOME`. It derives a unix socket path from
`MSB_HOME`, and that path must stay under 108 bytes.

## The harness image

| Option | Licence | Verdict | Why |
|---|---|---|---|
| Keep the Dockerfile; add `package.json` and `package-lock.json` for the harnesses, `npm ci`; `FROM ubuntu:24.04@sha256:...`; Dependabot for npm, docker and github-actions | Dependabot MIT | **adopt** | We built it. The size and the build time are the same as today. Each package has a pin with an integrity hash, and the base has a pin by digest. It retires the npm half of `bump-harness-versions.sh`. The Node version is an `ARG`, which Dependabot does not bump, so that half stays. pi needs a lock of its own, and Muse Code stays a script step |
| numtide/llm-agents.nix with nix2container | MIT; Apache-2.0 | **learn from** | We built it: all nine harnesses, reproducible to the image id, with Muse Code pinned by hash. But it is a third larger. It is ahead of our pins on eight of nine (so we would have to take numtide's versions, or override each). 281 of 288 store paths come from numtide's cache. Nix itself (LGPL-2.1) is in the build |
| Docker's sandbox kit spec and kits | Apache-2.0 (the `sbx` tool is proprietary) | **borrow** | Credential and allowlist declarations for each harness. Their harness versions are not pinned |
| apko and melange (Wolfi) | Apache-2.0 | learn from | Wolfi packages only Codex and Node of our set |
| Dev Container Features and CLI | MIT | reject | The lock pins the feature, not the tool that it installs. Anthropic's feature has no version option. There is no WSL containers driver |
| Anthropic's Claude Code devcontainer | all rights reserved | out | It also keeps a network, and it needs `NET_ADMIN` |

On 20 October, we built all three and ran them through the contract check and the harness smoke
under podman (`spikes/harness-containers/image-lockfile/`, `image-nix/`). Each passed as the
podman baseline does. Each failed only the vsock check, which belongs to the runtime, not to
the image:

| Image | Build | Size | Pinned | Versions |
|---|---|---|---|---|
| today's Dockerfile | 320 s | 3.15 GB | the 7 npm harnesses' own versions; base by tag | our pins |
| with the lockfile | 313 s | 3.15 GB | 107 lockfile packages, each with an integrity hash; base by digest | our pins |
| llm-agents.nix, nix2container | 360 s from an empty store, plus 116 s to load; 61 s unchanged | 4.08 GB | the flake lock, every store path by hash | numtide's, newer than ours on eight of nine |

The lockfile does less than it seems to. Together, the seven npm harnesses install only 30 or
31 packages. Most of these ship as bundles or as binaries for each platform. Thus most lock
entries are platform variants that never install. The large set that is not pinned is pi: 119
packages through `npm -g`. pi needs its own lock, because it installs with `--ignore-scripts`,
and the postinstall of Claude Code has to run. Both npm builds report five install scripts "not
covered by allowScripts". These scripts ran under npm 11.19. Before the next Node bump, check
whether npm 12 blocks them. The proposed `dependabot.yml` is in the lockfile spike directory,
not yet in `.github/`.

The repository has GitHub Actions workflows and no `.github/dependabot.yml`. The research
reports that the Dependabot page of ASF Infra requires one for the github-actions ecosystem, on
each repository that uses Actions. The research agent read that page. We did not check it again
here.

## The egress, and who else meters tokens

A third party could take these parts:
- the CONNECT allowlist, with an address guard;
- credential injection;
- TLS termination, to meter a tunnel;
- metrics.

These parts stay ours:
- the topology of socket and relay;
- the label check (it needs the read history of the session);
- metering and limits for each session;
- `/mcp`, which `dm_server` serves.

We read the source tree of each project. These meter tokens, from streamed responses too:
agentgateway, Agent Router (formerly Envoy AI Gateway), LiteLLM's MIT part, agentsh, Busbar,
Bifrost, AISIX, Plano and GoModel. ccusage reads the tokens from harness logs afterwards. These
proxies are on the sandbox side: sandbox-runtime, Codex's network proxy, iron-proxy, OpenShell,
Gondolin, microsandbox, Docker Sandboxes, Agent Substrate. Each of them injects credentials,
holds an allowlist, and has a hook before a request goes out. None of them counts tokens.

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

**agentgateway** is the closest to ours. It is a Rust binary for all three platforms. It does
these things:
- It reads input, output and cache tokens from Anthropic and OpenAI responses, as JSON
  and as streams.
- It prices the tokens from a catalog.
- It enforces money or token budgets.
- It injects credentials.
- It exposes hooks.
- It exports gen_ai metrics.
- It ships a Substrate gateway mode.

It has no unix-socket listener. It has no session unit (a CEL key that gets a session header
would serve). It has no label rule (an ext_authz call back to us). It would be in front of our
`/mcp`, and would not serve it. It could do the model forwarding behind our egress. But then a
second process holds the network, and both processes meter each call. This is worth the cost
only if we want its money budgets and Prometheus metrics more than we want one process fewer. We
reject Envoy itself. It is a large C++ daemon with a config language, beside a Rust binary whose
only job is this. Its `resolved_address_filter` is the idea to take for the CONNECT fix.
Infisical Agent Vault (MIT) does placeholder-key injection on a CONNECT path. A metered Copilot
tunnel would need this shape.

## Whole platforms

One fact controls this layer. All containers in a Kubernetes pod share one network namespace.
Thus a pod alone cannot give "harness offline, egress sidecar online". The institutional shape
holds point 1 only in one of two conditions:
- Our Linux backend runs inside the harness container. This needs a seccomp profile that allows
  `unshare`, `hostUsers: false`, or Kata.
- The egress runs in its own pod, and the harness pod reaches it over TCP behind a deny-all
  policy.

We got the answer on kind (rootless podman, Kubernetes 1.37, agent-sandbox 1.0.7). We ran the
contract check in the harness container
([spikes/harness-containers/agent-sandbox](../spikes/harness-containers/agent-sandbox/)):

| Layout | Result |
|---|---|
| A: harness and egress sidecar in one pod | point 1 fails: the harness reaches the API server, cluster DNS, other pods and the internet |
| B: the same pod, our namespaces backend inside the harness container, default seccomp | `unshare` refused |
| B, seccomp Unconfined | the network is cut, but vsock is open, as in our own backend |
| **B, a Localhost seccomp profile: the runtime default plus `unshare`, `setns`, `clone` and mounting, still refusing `AF_VSOCK`** | **every check passes** on the rootless node of the laptop, and also from a `SandboxTemplate` through a warm pool and a claim. On the rootful node of GitHub's runner, a mount of `/proc` fails: see below |
| B, `hostUsers: false` | the pod does not start: on a rootless node for two reasons, and on a rootful one because `runc` may not mount `sysfs` |
| C: the egress in its own pod, the harness pod deny-all but to it | kube-network-policies, which kindnet runs, enforces the policy. But the route out is pod-network TCP, not a socket. Thus point 1 holds only as far as the network plugin holds. Any pod that a policy does not stop can reach the egress Service |

Thus the institutional shape is B, where the node allows it:
- the egress as a native sidecar, with the socket in a shared `emptyDir`;
- our Linux backend inside the harness container;
- a seccomp profile on each node. A cluster administrator must do this step (the Security
  Profiles Operator, for example).

kind itself needed no root here. It needed only the podman log driver set to `k8s-file`. On the
second node that we tested, the mount namespace of B could not mount `/proc` (below). Thus
"where the node allows it" is not yet "on any node".

`hostUsers: false` had two problems on the rootless node.

**The kubelet's id ranges.** If the node's `/etc/subuid` has no `kubelet` entry, the kubelet
picks the 65,536 ids of each pod at random from the whole 32-bit range. It gave one pod
2,558,984,192 and another 1,137,049,600. The node maps only the ids that its user got. 16
million more ids on the host changed nothing. containerd's `setresuid` to such an id fails
with `invalid argument` (`pkg/sys/unshare_linux.go:167`). A range for the kubelet inside the
node's map fixed it. This needed a `kubelet` user, `getsubids` installed, and
`kubelet:131072:7208960` (110 pods of 65,536).

**Idmapped mounts.** Then containerd builds the root of the pod with an idmapped overlay.
`mount_setattr` refused it with `operation not permitted`. The node's `/var` is a podman volume
on the host's ext4. The rootless node holds no privilege over that filesystem. An idmapped
mount needs that privilege.

Thus `hostUsers: false` needs a rootful node. **On a rootful node, it still did not start.** The
kind job of the workflow ran the node under Docker on GitHub's Ubuntu 26.04 runner (run
`40019093054`). The uid map of the node was the whole range, so the node was rootful. Both
`hostUsers: false` pods failed to create their sandbox:

```text
runc create failed: unable to start container process: error during container init:
error mounting "sysfs" to rootfs at "/sys": ... flags=MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC:
operation not permitted
```

The next run (`40042405694`) printed the details of the node:
- kernel 7.0.0-1017-azure;
- containerd 2.3.4;
- runc 1.4.3;
- a `/sys` with kind's own files bound over two of its entries,
  `/sys/devices/virtual/dmi/id/product_name` and `product_uuid`.

The kernel mounts a new `sysfs` in a user namespace only in one condition. Each mount over the
`sysfs` that is already visible there must be on an empty directory (`mnt_already_visible` in
`fs/namespace.c`). These two mounts cover files. That is one candidate cause. It comes from the
kernel's rule, not from a run without kind's two mounts. We did not rule out another candidate.
`sysfs` also needs the mounter to hold the user namespace that owns its network namespace. This
fails if the pod's network namespace was made outside it. If the first candidate is the cause,
`hostUsers: false` cannot start on a kind node, and a real node is untouched. No run shows
either.

The same run started the other layouts after the runner saved one platform of the image. The
reason: Docker's store had saved an index that named platforms for which it held no layers. A
and C gave the same results as on the laptop. Each `unshare -U` in B failed when it wrote its
uid map. The cause is Ubuntu's AppArmor, which refuses unprivileged user namespaces on the
runner. Pull request 3 addresses this setting for our launcher.

With that setting off (run `40045349552`), B got as far as the laptop's B for the network.
`unshare -Urn` with the Localhost profile passed every check except "only loopback". A run
without a mount namespace cannot pass that check, because `/sys` still shows the pod's
interfaces. Then the full shape of our backend, `unshare -Urmnpf --mount-proc`, failed in each B
pod and in the claim of the warm pool: `unshare: mount /proc failed: Operation not permitted`.
On the laptop it passed. There are two candidate causes, and we tested neither:
- The same kernel rule, for `proc`. The runtime covers files under the `/proc` of each
  container (`/proc/kcore` and others).
- The runtime's AppArmor profile for pods refuses `mount`. This profile applies on the runner
  and not under WSL2.

A pod with `appArmorProfile: Unconfined` would tell the two candidates apart. No run has
shown yet whether `hostUsers: false` alone admits `unshare` under the default profile.

| Candidate | Licence | Verdict | Why |
|---|---|---|---|
| kubernetes-sigs/agent-sandbox | Apache-2.0 | **target** for the institutional shape | A full PodSpec for each sandbox, warm pools, claims, expiry. It ships a recall-memory example similar to our design |
| Agent Substrate | Apache-2.0 | **target** later, learn from now | Agents in gVisor or Cloud Hypervisor on Kubernetes. All egress goes through one replaceable gateway with an allowlist and credential injection, and our egress could be that gateway. Pre-1.0. It needs a cluster, PostgreSQL and an object store. No host bind mounts, so no point 2 on a laptop. Linux nodes only |
| OpenSandbox | Apache-2.0 | **borrow** | The closest open design: an egress sidecar for each sandbox that holds credentials, from Docker or Podman to Kubernetes. Its egress is a transparent intercept, not a socket |
| NVIDIA OpenShell | Apache-2.0 | **learn from** | The same architecture as ours: no network in the sandbox, one socket to a supervisor, injection, a hook, MCP checks, OTel. It needs a gateway daemon, and Podman 5 or KVM |
| E2B (SDK and runtime) | Apache-2.0 | learn from | A privileged Docker stack, KVM, about 12 GiB. No host socket or bind mounts |
| Dagger, container-use | Apache-2.0 | reject | No network-off. container-use leaves the agent on the host |
| OpenHands, SWE-ReX | MIT | reject | They need a TCP port into the sandbox |
| Daytona, Coder, Arrakis | AGPL-3.0 | **out** | Daytona's core repository is archived, and development moved to private |
| DevPod | MPL-2.0 | out (Category B) | |
| Docker Sandboxes | proprietary | **out** | |
| Hosted services (Cloudflare, Vercel, Modal, Runloop, Blaxel, Morph, Fly Sprites, Together) | open SDKs only | out of scope | A laptop-first, self-hosted reference cannot depend on them |

In this report, "Substrate" means Agent Substrate. The other projects with that name are
unlicensed, hosted, orchestration above the harness, or the Polkadot framework.

## What was not done

- Apple `container`, and anything else that needs a Mac's virtualization.
- gVisor's `kvm` platform on bare metal. Here it ran nested under Hyper-V.
- agent-sandbox with `hostUsers: false` on a node without kind's two files over `/sys`: a
  real cluster's node, or kind with them unmounted.
- Layout B's full shape on a rootful node with the pod's AppArmor profile `Unconfined`. This
  run would show which of the two candidates refuses `/proc`.
- A real call through nsjail and fcage. Their builds are gone, with the scratch directory. They
  bind the socket in exactly as podman does, and under podman the call completed.
- microsandbox again, after a fix for #1878. The test is `harness-call.sh` through
  `run-microsandbox-sdk.sh`.
