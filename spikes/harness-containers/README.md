# Harness container spikes

Throwaway scripts behind [docs/harness-containers.md](../../docs/harness-containers.md):
each candidate box is given the same session-shaped fixture and runs the same check, so the
results compare. Nothing here is built, tested, or shipped by the workspace.

| File | What it is |
|---|---|
| `contract-check.py` | runs inside a box; one line per assertion of contract points 1 and 2, exit 1 if any failed. Read-only is the mount's flag, never a refused write, which file mode alone would give |
| `fixture.sh` | the host side: a scratch tree at the launcher's layout, a stand-in egress on a unix socket that logs each request, a marker in a host home that must stay invisible. Its EXIT trap, set before anything is copied in, removes the tree on an error or a signal |
| `harness-smoke.sh` | runs inside a box with Node: three harnesses start, and Claude Code's calls reach the egress through a loopback relay |
| `harness-call.sh` | runs inside a box: one real Claude Code call through a loopback relay (Python where the image has it, else Node; vsock where `CHECK_SOCKET` says so), with `fixture.sh`'s `UPSTREAM` and `LOGIN` set |
| `run-host.sh` | the control: the check unconfined on the host, where it must fail |
| `run-amem.sh` | the baseline: our own Linux backend, `amem launch --harness command` |
| `run-seatbelt.sh` | our Seatbelt backend on macOS, `amem launch --harness command` with the real egress, the check told each host path; on Linux it runs the namespaces backend |
| `seatbelt-steps.sh` | where a launch under Seatbelt stops: `amem launch` with commands of growing reach, one exit code each |
| `run-podman.sh` | rootless podman, `--network none` |
| `run-nsjail.sh` | nsjail, with a seccomp rule refusing vsock and io_uring |
| `run-fcage.sh` | ferroday-cage's `fcage`, with the same rule |
| `run-gvisor.sh` | gVisor `runsc --rootless --network=none --host-uds=open` from a generated OCI spec |
| `run-microsandbox.sh` | microsandbox's `msb`, a libkrun microVM, the egress mapped to vsock port 5000 |
| `run-microsandbox-sdk.sh`, `msb-probe/` | the same through microsandbox's Rust SDK, for `disable_network()`, which the CLI cannot express; a standalone crate, outside the workspace |
| `bench.sh` | runs inside a box with Node: harness-shaped work, timed, or FAIL where a task did not complete |
| `bench-compare.sh` | `bench.sh` under runc, gVisor, gVisor with cached mounts, and gVisor's `kvm` platform where `/dev/kvm` is open, interleaved; a table of medians against runc, leaving out a task that failed in any box |
| `agent-sandbox/` | agent-sandbox on kind: three pod layouts, a seccomp profile that allows our backend inside a pod, a warm pool; its own README |
| `image-lockfile/`, `image-nix/` | the harness image with an npm lockfile and a digest-pinned base, and from llm-agents.nix; each with its own README |
| `research/` | the per-layer reports the doc was written from, and the brief their agents worked to |

## Getting the candidates

None needs root, except that microsandbox and gVisor's `kvm` platform need read and write on
`/dev/kvm`: membership of the `kvm` group (`sudo usermod -aG kvm $USER`), picked up without a
new login by `sg kvm -c '...'`. Put each download in an empty directory of its own.

```sh
# gVisor: the release tarball; runsc needs the sidecars that unpack beside it
gh release download release-20261012.0 -R google/gvisor -p gvisor-x86_64.tar.bz2 -p SHA256SUMS
grep x86_64.tar.bz2 SHA256SUMS | sha256sum -c && mkdir x && tar -xjf gvisor-x86_64.tar.bz2 -C x
export RUNSC=$PWD/x/runsc

# microsandbox: msb and libkrunfw, laid out as its installer lays them out. MSB_HOME's path
# goes into a unix socket path, which must stay under 108 bytes: link it from somewhere short
gh release download v0.7.8 -R superradcompany/microsandbox -p microsandbox-linux-x86_64.tar.gz -p checksums.sha256
grep linux-x86_64.tar.gz checksums.sha256 | sha256sum -c && tar -xzf microsandbox-linux-x86_64.tar.gz
mkdir -p bin lib && install -m755 msb bin/ && cp libkrunfw.so.* lib/
export MSB=$PWD/bin/msb MSB_HOME=$PWD

# ferroday-cage: the CLI from crates.io
cargo install --locked ferroday-cage-cli --version 0.5.0 --root "$PWD/fcage"
export FCAGE=$PWD/fcage/bin/fcage

# nsjail: from master, built in a container (3.6 needs libnl-3, which is LGPL)
podman run --rm -v "$PWD:/out:Z" docker.io/library/ubuntu:24.04 bash -c '
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    git g++ make pkg-config bison flex protobuf-compiler libprotobuf-dev >/dev/null
  git clone -q --depth 1 --recurse-submodules https://github.com/google/nsjail /src
  cd /src && make -j8 >/dev/null && cp nsjail /out/
  for l in $(ldd nsjail | awk "/protobuf|absl/{print \$3}"); do cp -L $l /out/; done'
export NSJAIL=$PWD/nsjail LD_LIBRARY_PATH=$PWD
```

`run-amem.sh` uses `target/debug/amem` (`cargo build -p amem`), or `AMEM`.

The SDK probe builds outside the workspace, into a target directory of its own:

```sh
(cd msb-probe && CARGO_TARGET_DIR=/some/scratch/msb-target cargo build --release)
export MSB_PROBE=/some/scratch/msb-target/release/msb-probe
```

## In CI

`.github/workflows/harness-spikes.yml` runs what this machine could not: `run-seatbelt.sh` on a
macOS runner, with Seatbelt's denials from the system log; `agent-sandbox/run.sh` with
`ENGINE=docker` on an Ubuntu runner, where the node is rootful and `hostUsers: false` can
start; and `run-gvisor.sh` and `bench-compare.sh` with `/dev/kvm` opened on an Ubuntu runner,
which is a VM, so kvm is nested there too. By hand (`gh workflow run harness-spikes.yml`, once
the file is on the default branch), and on a pull request that touches the spikes. Each job's
log is in the run's summary and its artifacts.

## Running

`./run-<box>.sh` runs the check. `run-podman.sh` and `run-gvisor.sh` take `ROOTFS`, an
unpacked root filesystem, and a command in place of the check; the harness smoke wants a
rootfs with Node and the harnesses, which podman can make and export:

```sh
podman run --name h docker.io/library/node:24-slim npm install -g \
  @anthropic-ai/claude-code@2.1.296 @openai/codex@0.162.1 @google/gemini-cli@0.63.0
mkdir rootfs && podman export h | tar -x -C rootfs && podman rm h
ROOTFS=$PWD/rootfs ./run-gvisor.sh sh /work/harness-smoke.sh
```

A real call needs a Claude Code login on the host. `fixture.sh` copies its access token, without
the refresh token, into the box's home, and forwards to the API:

```sh
UPSTREAM=https://api.anthropic.com LOGIN=$HOME/.claude/.credentials.json \
    ROOTFS=$PWD/rootfs ./run-podman.sh sh /work/harness-call.sh
# an image with no Claude Code (microsandbox pulls python:3.12-slim): copy a binary in
UPSTREAM=... LOGIN=... CLAUDE_BIN=$(readlink -f ~/.local/bin/claude) \
    ./run-microsandbox-sdk.sh sh /work/harness-call.sh
```

## Results, 19, 20 and 23 October 2026

Linux on WSL2, kernel 6.18.40, x86_64, podman 4.9.3 with runc. FAIL lines only; every other
assertion passed. Every row is the corrected check (read-only by the mount's flag), rerun on
23 October; the first version passed `/usr` and the memory binary on file mode alone, even
unconfined.

| Box | Failed |
|---|---|
| host, unconfined (control) | only loopback; no TCP out; no name resolution; no vsock; projection, system and memory binary read-only; no rw mount under `/usr`; host marker invisible |
| `amem` namespaces (ours) | no vsock socket; no rw mount under `/usr` (`/usr/lib/wsl/lib`, `/usr/lib/modules/...`) |
| rootless podman, runc | no vsock socket |
| nsjail, built from master (`4ff54a6`, 2 October), with the rule | none |
| nsjail without the rule (`NSJAIL_SECCOMP=`) | no vsock socket |
| fcage 0.5.0, with the rule | none |
| fcage without the rule (`FCAGE_SECCOMP=`) | no vsock socket |
| gVisor `release-20261012.0`, rootless, systrap | none; the workload runs as uid 0 inside, see below |
| the same on `--platform=kvm` | none (and without `/dev/kvm` access it refuses to start: `error opening KVM device file (/dev/kvm): permission denied`) |
| microsandbox 0.7.8, `msb run --net none` | only loopback (`eth0`, `dummy0`); system read-only (the image's root is a read-write mount) |
| microsandbox 0.7.8, SDK `disable_network()` | only loopback (`dummy0`: down, no address, no route; no virtio-net device in the guest); system read-only (a read-write mount) |

Harness smoke, on one rootfs (node:24-slim with Claude Code 2.1.296, Codex 0.162.1, Gemini
CLI 0.63.0): all three start under podman and under gVisor, and Claude Code's calls reach
the stand-in egress through the loopback relay under both (7 and 6 calls in 20 seconds, each
also logged by the stand-in outside the box; it retries the stand-in's 401, so the run ends
on the timeout).

Real call, `harness-call.sh`, 23 October, Claude Code on a subscription login, one word asked:

| Box | Result |
|---|---|
| `amem launch --harness claude` (ours, the real egress) | answered; metered 1 call, 2 in, 4 out, 10,182 cache |
| rootless podman, runc (npm 2.1.296; and the native 2.1.304 in `python:3.12-slim`) | answered, `POST /v1/messages -> 200` |
| gVisor, systrap and kvm | answered, `POST /v1/messages -> 200` |
| microsandbox, SDK, no NIC (native 2.1.304 in `python:3.12-slim`) | timed out at 120 s, nothing reached the egress |

microsandbox's vsock route, probed with raw `AF_VSOCK` writes to the stand-in:
- 16, 24 and 32 KB in one write: answered at once.
- 48, 64, 128 and 512 KB in one write: no answer in 60 s; the stand-in saw the start of the
  request and nothing after.
- 128 KB in 8 or 4 KB writes: answered.
- 1 MB in 16 KB writes: stalled after 262,080 bytes.

This is superradcompany/microsandbox#1878, open, and not addressed in 0.7.9. Two more things
the runs needed:
- microsandbox's `exec` returns only when every holder of the command's output has exited, so
  `harness-call.sh` stops its relay at the end.
- `amem launch` with a root whose egress socket path came to 146 bytes failed on the bind
  mount (`ENOENT`), not on the socket; see the doc's fourth gap.

Overhead, `bench-compare.sh`, median of three interleaved rounds, 23 October:

| Task | runc | gVisor | gVisor, mounts cached | gVisor, kvm platform |
|---|---|---|---|---|
| Node start, 20 times | 560 ms | 3.1x | 3.2x | 4.1x |
| `claude --version`, 5 times | 60 ms | 3.6x | 3.8x | 4.8x |
| 10,000 small files written, read and removed in the working tree | 776 ms | 5.6x | 5.2x | 4.5x |
| the same in `/tmp` | 360 ms | 2.9x | 3.1x | 6.2x |
| copy a `node_modules` tree in `/tmp` | 118 ms | 2.2x | 2.1x | 2.2x |
| a CPU loop in Node | 1,056 ms | 1.2x | 1.2x | 1.2x |

On 20 October, without the kvm column: runc 666, 74, 899, 425, 138 and 1,283 ms; gVisor 3.2x,
3.7x, 6.2x, 3.2x, 2.1x, 1.1x; cached 3.1x, 3.5x, 6.1x, 3.2x, 2.0x, 1.1x. The `kvm` platform ran
nested, WSL2 being a Hyper-V guest.

gVisor rootless maps the caller to root inside and refuses a uid mapping of its own: with
no user namespace in the spec it says the mappings need one, and with one it cannot fork its
gofer (`fork/exec /proc/self/exe: operation not permitted`). So the box runs as uid 0, which
is the caller outside, rather than the layout's uid 1000.
