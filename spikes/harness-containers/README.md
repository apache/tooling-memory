# Harness container spikes

Throwaway scripts behind [docs/harness-containers.md](../../docs/harness-containers.md):
each candidate box is given the same session-shaped fixture and runs the same check, so the
results compare. Nothing here is built, tested, or shipped by the workspace.

| File | What it is |
|---|---|
| `contract-check.py` | runs inside a box; one line per assertion of contract points 1 and 2, exit 1 if any failed |
| `fixture.sh` | the host side: a scratch tree at the launcher's layout, a stand-in egress on a unix socket, a marker in a host home that must stay invisible |
| `harness-smoke.sh` | runs inside a box with Node: three harnesses start, and Claude Code's calls reach the egress through a loopback relay |
| `run-host.sh` | the control: the check unconfined on the host, where it must fail |
| `run-amem.sh` | the baseline: our own Linux backend, `amem launch --harness command` |
| `run-podman.sh` | rootless podman, `--network none` |
| `run-nsjail.sh` | nsjail, with a seccomp rule refusing vsock and io_uring |
| `run-fcage.sh` | ferroday-cage's `fcage`, with the same rule |
| `run-gvisor.sh` | gVisor `runsc --rootless --network=none --host-uds=open` from a generated OCI spec |
| `run-microsandbox.sh` | microsandbox's `msb`, a libkrun microVM, the egress mapped to vsock port 5000 |
| `run-microsandbox-sdk.sh`, `msb-probe/` | the same through microsandbox's Rust SDK, for `disable_network()`, which the CLI cannot express; a standalone crate, outside the workspace |
| `bench.sh` | runs inside a box with Node: harness-shaped work, timed |
| `research/` | the per-layer reports the doc was written from, and the brief their agents worked to |

## Getting the candidates

None needs root, except that microsandbox needs read and write on `/dev/kvm`. Put each
download in an empty directory of its own.

```sh
# gVisor: the release tarball; runsc needs the sidecars that unpack beside it
gh release download release-20261012.0 -R google/gvisor -p gvisor-x86_64.tar.bz2 -p SHA256SUMS
grep x86_64.tar.bz2 SHA256SUMS | sha256sum -c && mkdir x && tar -xjf gvisor-x86_64.tar.bz2 -C x
export RUNSC=$PWD/x/runsc

# microsandbox: msb and libkrunfw, laid out as its installer lays them out
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

## Results, 19 and 20 October 2026

Linux on WSL2, kernel 6.18.40, x86_64, podman 4.9.3 with runc. FAIL lines only; every other
assertion passed.

| Box | Failed |
|---|---|
| host, unconfined (control) | only loopback; no TCP out; no name resolution; no vsock; projection read-only; no rw mount under `/usr`; host marker invisible |
| `amem` namespaces (ours) | no vsock socket; no rw mount under `/usr` (`/usr/lib/wsl/lib`, `/usr/lib/modules/...`) |
| rootless podman, runc | no vsock socket |
| nsjail, built from master on 19 October, with the rule | none |
| nsjail without the rule (`NSJAIL_SECCOMP=`) | no vsock socket |
| fcage 0.5.0, with the rule | none |
| fcage without the rule (`FCAGE_SECCOMP=`) | no vsock socket |
| gVisor `release-20261012.0`, rootless, systrap | none; the workload runs as uid 0 inside, see below |

Harness smoke, on one rootfs (node:24-slim with Claude Code 2.1.296, Codex 0.162.1, Gemini
CLI 0.63.0): all three start under podman and under gVisor, and Claude Code's calls reach
the stand-in egress through the loopback relay under both (7 and 6 calls in 20 seconds; it
retries the stand-in's 401, so the run ends on the timeout).

gVisor rootless maps the caller to root inside and refuses a uid mapping of its own: with
no user namespace in the spec it says the mappings need one, and with one it cannot fork its
gofer (`fork/exec /proc/self/exe: operation not permitted`). So the box runs as uid 0, which
is the caller outside, rather than the layout's uid 1000.
