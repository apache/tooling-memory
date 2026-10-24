# Layer: external runtimes as the harness boundary

Surveyed 19 October 2026. Every licence, date and star count below comes from a `gh api` call made
that day. Where a claim comes from a run on this machine (Linux, WSL2 kernel 6.18, x86_64, podman
4.9.3, runc 1.3.3), it says so. The run's scripts and logs are under
`$SCRATCH/research/spike/`
and `.../research/scripts/`.

Licence check, run for every repository:
`gh api repos/OWNER/REPO --jq '{full_name,pushed_at,archived,stargazers_count,lic:.license.spdx_id}'`
and `gh api repos/OWNER/REPO/releases/latest --jq '{tag_name,published_at}'`. Where the SPDX id was
null or NOASSERTION, the LICENSE or COPYING files were read.

## How the egress socket crosses each kind of boundary (contract point 1)

There are four patterns. Each runtime below uses one of them.

- **A. Bind the socket in (same kernel).** Our namespaces, runc, youki, crun, podman, and gVisor
  all bind-mount the socket. gVisor refuses host sockets by default: it needs `--host-uds=open`,
  which allows `connect(2)` on host unix sockets and nothing else
  (`runsc/config/flags.go:146`; values `none|open|create|all`). Point 1 holds unchanged, and the
  `amem relay` inside does not change.
- **B. Map a vsock port to the host socket (VM).** A unix socket file shared over virtiofs or 9p is
  not something the guest can `connect()` to across the VM boundary. This was not tested here. It
  is why every VMM offers a vsock route instead, and microsandbox's docs say that is what their
  route uses. The VMM maps guest `AF_VSOCK` (CID 2, port N) to a host unix socket:
  - Firecracker: the guest connects to port N, and the VMM connects to `<uds_path>_N` on the host
    (`docs/vsock.md`, "Guest-Initiated Connections").
  - Cloud Hypervisor: the same hybrid vsock (`--vsock cid=3,socket=...`).
  - libkrun: `krun_vsock_device_add_unix_port(handle, port, path, listen=false)` (`include/libkrun.h:498`).
  - microsandbox: `--vsock HOST_PATH:PORT`, or `.vsock(path, port)` in the SDK.
  - vfkit: `--device virtio-vsock`, exposed as a unix socket on the host.
  - Apple Containerization: `UnixSocketConfiguration` with direction `.into`. It relays the socket
    over vsock and presents it in the guest as a unix socket at the path you name, so `-v sock:path`
    just works.

  Point 1 holds if three things are true: the VM has no NIC, TSI (libkrun's socket hijack) is off,
  and the only vsock route is the one we map. The egress is then still the only process with the
  network. The cost is that `amem relay` needs an `AF_VSOCK` client mode (crate `vsock` or
  `tokio-vsock`, no `unsafe` of ours). Apple's relay is the exception: it presents a unix socket,
  so the relay does not change there.
- **C. Put the egress inside the same VM (the wslc pattern).** An egress container with the network
  and a harness container with `--network none` share a volume that carries the socket. This works
  in any VM runtime that runs two containers in one VM: podman machine, Lima/Colima, and wslc.
  Point 1 is the same as today. The egress becomes a Linux process in the guest, and the record has
  to be mounted into the VM.
- **D. TCP port forward or ssh socket forward (Lima, smolvm mediated egress).** The guest needs a
  network stack or sshd, and the route is TCP or ssh. Point 1 weakens to "a network exists, but is
  policed". To restore it, a second boundary would have to run inside the VM: our own namespaces
  backend, which works there because the guest is ordinary Linux.

On one image for all three platforms: the VM runtimes that boot OCI images can all run
`images/harness` unchanged. That is microsandbox, smolvm, BoxLite, Apple container, podman machine
and wslc. On Apple Silicon the guest is arm64, and today's Dockerfile hard-codes the
`linux-x64` Node tarball, so it would need a multi-arch build. gVisor, runc and youki already take
the unpacked rootfs (`amem image unpack`).

---

## gVisor (`runsc`)
- **Repo / licence (verified)**: https://github.com/google/gvisor; Apache-2.0; `gh api repos/google/gvisor` lic=`Apache-2.0`.
- **Form**: a single static Go binary (`runsc`) plus sidecar binaries in the same tarball (`gvisor-bin/gvisor_sentry`, `runsc-fd-parking` and others). It needs no root and no daemon with `--rootless`, and no KVM on the default `systrap` platform (`flags.go:103`).
- **Platforms**: Linux yes; macOS no; Windows no (WSL2 kernel yes, run here).
- **Mechanism**: the Sentry, a user-space kernel, serves every syscall. A Gofer process opens host files for it. The application never touches a host namespace.
- **Against our contract**:
  - Point 1 holds, run here. `--network=none` leaves loopback only (`/proc/net/dev`: `lo` alone), `connect(1.1.1.1:443)` gave `ENETUNREACH`, and `--host-uds=open` let the probe reach the bind-mounted egress socket (`unix: egress-ok`).
  - Point 2 holds, run here. An OCI `config.json` with a read-only root and `rbind` mounts put the tree at `/work` (rw, write visible on the host), the socket dir at `/run/memory`, a home at `/home/user` (rw), and `/opt/memory` read-only (`EROFS`). `/usr` was read-only.
  - Point 3: it replaces namespaces with a stronger kernel boundary. It holds "no root, no daemon", but adds a runtime binary.
  - Points 4 and 5: no.
  - Point 6: silent.
  - Found here: binding host `/usr` fails in rootless mode on WSL2 with `EINVAL`. `/usr` carries locked submounts (`/usr/lib/modules` overlay, `/usr/lib/wsl/drivers` 9p), and the gofer binds non-recursively (flags `0x1001`, log in `spike/dbg/`). Rooting from the unpacked image, which is our Linux default, or binding subdirectories avoids it.
  - Also: `runsc bwrap` is a bwrap-flag personality (`g3doc/user_guide/personalities/bwrap.md`). Its `--symlink` is unsupported and `--ro-bind /usr` hit the same `EINVAL`.
  - The rootless docs say `create` is not supported. `run` with `--network=none` worked here.
- **Maintenance**: pushed 2026-10-19; release `release-20261012.0` (2026-10-16); not archived; 19.7k stars. The documented `storage.googleapis.com/.../release/latest/x86_64/runsc` returned 404 today; the GitHub release tarball and `SHA256SUMS` worked.
- **If we adopted it**: it sits beside or under `linux.rs`. We would generate an OCI spec from the `Plan` and exec `runsc`, which replaces our own `unshare` and mounts. It adds about 100 MB of Go binaries to ship and pin, plus syscall-compatibility risk for Node, which was not tested here. One probe took 0.56 s wall time.
- **Verdict**: **adopt, as an optional hardened Linux backend** (`--backend gvisor`). It holds points 1 and 2 rootless and without KVM, as shown here.

## Firecracker (+ jailer)
- **Repo / licence (verified)**: https://github.com/firecracker-microvm/firecracker; Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: a single Rust binary VMM with a REST API on a unix socket. It needs `/dev/kvm`. The jailer starts as root to chroot, set cgroups and drop privileges (`docs/jailer.md`: "Drop privileges via setting the provided uid and gid").
- **Platforms**: Linux yes; macOS no; Windows no.
- **Mechanism**: a minimal KVM microVM with virtio-net, virtio-block, vsock and pmem only.
- **Against our contract**:
  - Point 1 holds by pattern B. A guest connect to port N reaches `<uds_path>_N` on the host. With no NIC configured, the guest has nothing else.
  - Point 2 breaks for live sharing. The tree has no virtio-fs (none in the source tree; block and pmem only), so the working tree and home would have to be block images, not shared directories.
  - Point 3: the jailer needs root.
  - Points 4 and 5: no.
- **Maintenance**: pushed 2026-10-19; v1.17.0 (2026-09-10); 37.5k stars.
- **If we adopted it**: it would replace the namespaces backend, and add a guest kernel, image-to-ext4 conversion, and `/dev/kvm`.
- **Verdict**: **reject**. There is no directory sharing, so the working tree cannot be bound live.

## Cloud Hypervisor
- **Repo / licence (verified)**: https://github.com/cloud-hypervisor/cloud-hypervisor. `gh api` lic=null. `LICENSES/` holds `Apache-2.0.txt`, `BSD-3-Clause.txt` and `CC-BY-4.0.txt`, read by listing; the code is Apache-2.0 and BSD-3-Clause (crosvm-derived). virtiofsd (https://gitlab.com/virtio-fs/virtiofsd): the GitLab API reports `apache-2.0`.
- **Form**: a single Rust binary VMM, plus one `virtiofsd` per share. It needs `/dev/kvm` or MSHV. Its docs `setcap cap_sys_admin` on virtiofsd (`docs/fs.md`).
- **Platforms**: Linux host yes. The README's "Windows 10/Server 2019" means guests, not hosts. macOS no.
- **Mechanism**: a KVM/MSHV VMM with virtio-fs, virtio-vsock (hybrid, unix-socket backed) and virtio-net.
- **Against our contract**:
  - Point 1 holds by pattern B (`--vsock cid=3,socket=...`, same `_PORT` convention), with no NIC.
  - Point 2 holds: virtio-fs gives each share at any guest path, and read-only is set in the guest mount.
  - Point 3 breaks: it needs KVM, a virtiofsd daemon per share with `CAP_SYS_ADMIN` in the documented setup, and a guest kernel.
  - Points 4 and 5: no. Apple's Containerization now uses it as its Linux backend (README line 40).
- **Maintenance**: pushed 2026-10-17; v54.0 (2026-10-14); 6.4k stars.
- **If we adopted it**: a Linux VM backend with more moving parts than libkrun (a VMM, virtiofsd per share, and a kernel).
- **Verdict**: **learn-from**. It is sound, but heavier than libkrun for the same pattern, and Linux only.

## Kata Containers
- **Repo / licence (verified)**: https://github.com/kata-containers/kata-containers; Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: a containerd shimv2 runtime, plus a hypervisor (QEMU, Cloud Hypervisor or Firecracker), a guest kernel and an agent. In "rootless VMM" mode, "the Kata shim and virtiofsd still run as root on the host" (`docs/how-to/how-to-run-rootless-vmm.md`).
- **Platforms**: Linux yes; macOS no; Windows no.
- **Mechanism**: every pod is a VM, driven by containerd or CRI-O through the shim.
- **Against our contract**:
  - Points 1 and 2: possible through the container manager.
  - Point 3 breaks: root shim, containerd or CRI-O, and KVM.
- **Maintenance**: pushed 2026-10-19; 4.2.0 (2026-09-15); 9.0k stars.
- **If we adopted it**: it adds containerd, a root shim, a hypervisor and a kernel.
- **Verdict**: **reject**. It is built for Kubernetes and needs root.

## libkrun (direct)
- **Repo / licence (verified)**: https://github.com/libkrun/libkrun (moved from containers/libkrun); Apache-2.0; `gh api` lic=`Apache-2.0`. The crates.io `libkrun` 1.19.6 also reports Apache-2.0. Its companion libkrunfw (https://github.com/libkrun/libkrunfw, `gh` NOASSERTION) holds `LICENSE-LGPL-2.1-only` and `LICENSE-GPL-2.0-only`. Its README says the bundled kernel is GPL-2.0-only and the library is LGPL-2.1-only, and that "other programs linking against this library are not required to be licensed under" either. Shipping it in binary form obliges us to ship the kernel source.
- **Form**: a C and Rust dynamic library that the host process links. It uses KVM on Linux and Hypervisor.framework on macOS arm64. WHP on Windows is "upcoming" upstream; the superradcompany fork carries `virtio/fs/windows`. No root and no daemon; it needs `/dev/kvm` access.
- **Platforms**: Linux yes; macOS yes (Apple Silicon); Windows partial (fork only).
- **Mechanism**: the process becomes a microVM with a bundled kernel (libkrunfw) that boots a rootfs directory or disk. It provides virtio-fs, vsock, and optional TSI or virtio-net (passt or gvproxy).
- **Against our contract**:
  - Point 1 holds by pattern B. Create the vsock device with no TSI flags (`KRUN_TSI_FLAGS_HIJACK_INET|HIJACK_UNIX` are opt-in, `libkrun.h:164`) and no net device, then `krun_vsock_device_add_unix_port(port, "/host/egress.sock", false)`. TSI must stay off, because it runs guest sockets in the VMM's host network context (`docs/networking.md`).
  - Point 2 holds: `virtiofs` devices with a read-only variant (`libkrun.h:209-222`), mounted at fixed guest paths.
  - Point 3: it adds a VMM, a guest kernel and `/dev/kvm`, but no daemon.
  - Point 4: a real VM boundary on macOS instead of Seatbelt.
- **Maintenance**: pushed 2026-10-19; v1.19.6 (2026-09-29); 2.8k stars.
- **If we adopted it**: a VM backend linked in-process. A link to a C library needs a binding crate, which is our rule for FFI, the way wslc-api is. It is the same engine under microsandbox, smolvm, krunvm, krunkit, BoxLite, Gondolin's `krun` backend and Lima's `krunkit` driver.
- **Verdict**: **borrow / spike behind microsandbox first**. It is the common engine, but the libkrunfw LGPL/GPL obligations need a licence decision.

## krunvm
- **Repo / licence (verified)**: https://github.com/libkrun/krunvm; Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: a CLI over libkrun and buildah (images). It needs KVM or HVF.
- **Platforms**: Linux yes; macOS yes; Windows no.
- **Mechanism**: it turns an OCI image into a libkrun microVM.
- **Against our contract**: its networking is TSI (host network), and the CLI exposes no vsock-to-unix-socket mapping, so point 1 is not available without patching. Point 2: `-v` volumes.
- **Maintenance**: pushed 2026-09-15; v0.2.7 (2026-08-25); 1.8k stars.
- **If we adopted it**: it adds buildah and libkrun.
- **Verdict**: **reject**. It is thin, and the route we need is not exposed.

## krunkit
- **Repo / licence (verified)**: https://github.com/libkrun/krunkit; Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: a CLI. It is the macOS libkrun launcher used by podman machine and Lima.
- **Platforms**: Linux no; macOS yes; Windows no.
- **Mechanism**: a libkrun VM with GPU (venus). Not examined further.
- **Against our contract**: it is a VM launcher for pattern C on macOS, through podman or Lima.
- **Maintenance**: pushed 2026-10-14; v1.3.2 (2026-07-03).
- **If we adopted it**: indirectly, through podman machine or Lima.
- **Verdict**: **reject as a direct dependency**.

## microsandbox
- **Repo / licence (verified)**: https://github.com/superradcompany/microsandbox (formerly microsandbox/microsandbox); Apache-2.0; `gh api` lic=`Apache-2.0`, and workspace `Cargo.toml` `license = "Apache-2.0"`. It depends on `msb_krun =0.1.41` (crates.io: Apache-2.0, repository zerocore-ai/libkrun, which is the superradcompany/libkrun fork, Apache-2.0) and vendors `libkrunfw` (LGPL-2.1 plus a GPL-2.0 kernel, as above).
- **Form**: a Rust library (`microsandbox` crate) with SDKs in TS, Python, Go and Ruby, and the `msb` CLI. Per-sandbox host runtime processes, "No setup server. No long-running daemon". It keeps state in `MSB_HOME` (SQLite, image cache). It needs KVM, HVF or WHP; no root.
- **Platforms**: Linux yes (KVM); macOS yes (Apple Silicon); Windows yes (WHP). Release assets exist for all three.
- **Mechanism**: libkrun microVMs that boot OCI images or a local rootfs directory (`examples/rust/root-bind`). A guest `agentd` speaks to the host over a virtio console.
- **Against our contract**:
  - Point 1 holds by pattern B. The Rust and TS `network(|n| n.enabled(false))` removes the network device. The CLI `--no-net` keeps the device and denies traffic by policy (`docs/networking/overview.mdx`). `--vsock /host/egress.sock:5000` gives the guest `AF_VSOCK` to CID 2, port 5000, and on Windows it can map a named pipe (`docs/networking/host-sockets.mdx`). Our relay would need a vsock client mode.
  - Point 2 holds: `--mount-dir SRC:DEST[:ro,noexec]` at any guest path, through virtiofs.
  - Point 3: it replaces namespaces with a VM. No root and no daemon, but KVM, a kernel, and its own state directory.
  - Points 4 and 5: one OCI image on all three platforms, an arm64 build on Macs.
  - Point 6 overlaps rather than replaces: it has its own egress allow-lists, secret placeholders and TLS interception. We would leave those off.
  - It is marked "beta software. Expect breaking changes".
- **Maintenance**: pushed 2026-10-19; v0.7.8 (2026-10-09); 8.7k stars.
- **If we adopted it**: one backend for Linux, macOS and Windows in place of namespaces, Seatbelt and wslc. It adds libkrun, libkrunfw (kernel source obligation), agentd in the guest, an `MSB_HOME` database, and a fast-moving beta API.
- **Verdict**: **spike (borrow candidate)**. It is the only Apache-2.0 runtime whose documented features give point 1 (no NIC plus one host-socket route) and point 2 on all three platforms with one image.

## BoxLite
- **Repo / licence (verified)**: https://github.com/boxlite-ai/boxlite; Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: an embeddable Rust library with Python, Node, Go and C SDKs, an optional REST server and a CLI. No root and no daemon. Built on `libkrun-sys` and `libgvproxy-sys`.
- **Platforms**: Linux yes (KVM); macOS yes (Apple Silicon); Windows only through WSL2 with KVM.
- **Mechanism**: a libkrun VM per box. Networking is gvproxy (default) or libslirp. Host directories are shared over virtiofs.
- **Against our contract**:
  - Point 1 is partial. `NetworkSpec::Disabled` exists (`src/boxlite/src/runtime/options.rs:669`), but no public option maps a host unix socket into the guest was found in `options.rs` or `docs/concepts/networking.md`. A disabled network also refuses ports. The route would need adding.
  - Point 2 holds: `VolumeSpec { guest_path, read_only }`.
  - Windows is WSL2 only, so no native Windows.
- **Maintenance**: pushed 2026-10-19; v0.10.7 (2026-10-18); 2.6k stars.
- **If we adopted it**: the same engine as microsandbox, without the socket route.
- **Verdict**: **learn-from**. The route we need is missing.

## smolvm (current; formerly ERA)
- **Repo / licence (verified)**: https://github.com/smol-machines/smolvm; Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: a CLI plus in-process SDKs, "no daemon". libkrun with its own libkrunfw. Needs KVM, HVF or WHP.
- **Platforms**: Linux yes; macOS yes (Apple Silicon); Windows yes (WHP).
- **Mechanism**: libkrun microVMs from OCI images, with checkpoints and branching. Networking is off by default.
- **Against our contract**:
  - Point 1 is partial. Networking is off by default, but the only host-socket forwards found are named ones (`ssh_agent`, `docker_socket` in `docs/smolfile.md`), not a general path. "Mediated egress" (`docs/mediated-egress.md`) would send every guest TCP flow to a loopback decider of ours. That is pattern D (network on, policed), not available on Windows, and the decider is TCP, not a unix socket.
  - Point 2: volumes.
- **Maintenance**: pushed 2026-10-19; v1.30.1 (2026-10-17); 6.8k stars.
- **If we adopted it**: like microsandbox, but the socket route would have to be added.
- **Verdict**: **learn-from**. The mediated-egress protocol is worth reading as a design. Second choice behind microsandbox.

## Apple `container` / Containerization (macOS)
- **Repo / licence (verified)**: https://github.com/apple/container and https://github.com/apple/containerization; both Apache-2.0 (`gh api` lic=`Apache-2.0`).
- **Form**:
  - `container` is a Swift CLI plus launchd-managed services: `container-apiserver` starts on `container system start`, with XPC helpers and one `container-runtime-linux` per container (`docs/technical-overview.md`).
  - Containerization is a Swift package that can be embedded with no daemon. It needs the virtualization entitlement.
  - Both need macOS 26 on Apple Silicon (README).
- **Platforms**: Linux partial (the framework only, through a Cloud Hypervisor backend); macOS yes; Windows no.
- **Mechanism**: one lightweight VM per container (Virtualization.framework) with `vminitd` as the guest init.
- **Against our contract**:
  - Point 1 holds by pattern B, with no change to our relay. `--network none` was merged on 2025-10-21 (PR #739). A host unix socket mounted with `-v` is relayed over vsock (`UnixSocketConfiguration`, direction `.into`), which is how `--ssh` works (`docs/host-integration.md`). `--publish-socket` is the other direction.
  - Point 2 holds: `-v` and `--mount ...,readonly` at any path, and `--read-only` root.
  - Point 3: it adds a launchd service, or a Swift helper binary of ours that links the framework.
  - Point 4: a real alternative to the dry-coded Seatbelt backend. It runs the same OCI image built for arm64.
- **Maintenance**: container pushed 2026-10-19, release 1.5.0 (2026-09-29), 50.8k stars. Containerization pushed 2026-10-19, tag 0.52.1.
- **If we adopted it**: a `--backend apple` beside or instead of Seatbelt. A Swift helper would be a second language in the build.
- **Verdict**: **spike on a Mac**. It is the cleanest pattern-B fit on macOS, needs no relay change, and uses the same image as wslc.

## Lima / Colima
- **Repo / licence (verified)**: https://github.com/lima-vm/lima, Apache-2.0 (`gh api`). https://github.com/abiosoft/colima, MIT (`gh api`).
- **Form**: CLIs plus a per-instance host agent (`limactl`). Full VMs with cloud-init, sshd and guest agent. Drivers: `qemu`, `vz`, `krunkit`, `wsl2`, `hcs`, `external` (`pkg/driver/`).
- **Platforms**: Linux yes; macOS yes; Windows partial (`wsl2` and `hcs` drivers present; maturity not assessed).
- **Mechanism**: a long-lived Linux VM with mounts (virtiofs, 9p or reverse-sshfs) and port or socket forwards over ssh or gRPC.
- **Against our contract**:
  - Point 1 by pattern D. `portForwards` with `guestSocket`/`hostSocket` and `reverse: true` (unix only, `templates/default.yaml:571-576`) can put the host egress socket inside, but the VM has user-mode networking. Holding point 1 means running our namespaces backend inside the VM, or pattern C.
  - Point 2: mounts at any guest path, with a `writable` flag.
- **Maintenance**: Lima pushed 2026-10-19, v2.2.1 (2026-10-03), 22.1k stars. Colima pushed 2026-10-13, v0.10.3 (2026-06-04).
- **If we adopted it**: it is a Linux host on macOS for our existing Linux backend, not a boundary.
- **Verdict**: **learn-from**. It is a VM manager, not a confinement boundary.

## runc (rootless)
- **Repo / licence (verified)**: https://github.com/opencontainers/runc; Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: a single Go binary OCI runtime. No daemon; rootless through `runc spec --rootless`.
- **Platforms**: Linux yes; macOS no; Windows no.
- **Mechanism**: kernel namespaces, cgroups and seccomp from an OCI `config.json`.
- **Against our contract**:
  - Point 1 holds, run here. A rootless spec plus a network namespace gave TCP `ENETUNREACH` and the egress socket reachable (`spike/runc.log`). The rootless spec bind-mounts host `/sys`, so `/sys/class/net` still lists the host's `eth0`. It is cosmetic, but a leak of host information.
  - Point 2 holds, run here: fixed paths, `ro` enforced (`EROFS` on `/opt/memory` and `/usr/bin`).
- **Maintenance**: pushed 2026-10-19; v1.5.2 (2026-09-25); 13.5k stars.
- **If we adopted it**: it replaces about 370 lines of `linux.rs` with a Go binary and generated JSON. It adds a binary to ship, and the same kernel boundary.
- **Verdict**: **reject**. It gives no isolation gain over what we have, and adds a dependency.

## youki (+ `libcontainer` crate)
- **Repo / licence (verified)**: https://github.com/youki-dev/youki; Apache-2.0 (`gh api`). crates.io `libcontainer` 0.7.0 is Apache-2.0.
- **Form**: a Rust OCI runtime binary, plus `libcontainer` as an embeddable Rust library. Rootless supported.
- **Platforms**: Linux yes; macOS no; Windows no.
- **Mechanism**: the same as runc, in Rust.
- **Against our contract**: points 1 and 2 hold the same way as runc (an OCI spec). Not run here.
- **Maintenance**: pushed 2026-10-19; v0.7.0 (2026-07-25); 7.6k stars.
- **If we adopted it**: `libcontainer` could replace `linux.rs`'s namespace and mount code in-process. It brings its own `unsafe` into the tree as a dependency, not ours.
- **Verdict**: **learn-from**. It belongs to the library layer's question more than this one.

## crun
- **Repo / licence (verified)**: https://github.com/containers/crun. `gh api` lic=`GPL-2.0`. `COPYING` is GPL v2, and `src/crun.c` says "version 2 of the License, or (at your option) any later version", so GPL-2.0-or-later. `COPYING.libcrun` is LGPL-2.1.
- **Form**: a C OCI runtime binary; libcrun is the library.
- **Platforms**: Linux yes.
- **Mechanism**: as runc. It is podman's default runtime, though this machine's podman uses runc.
- **Against our contract**: as runc.
- **Maintenance**: pushed 2026-10-19; 1.30.1 (2026-09-25).
- **If we adopted it**: not allowed.
- **Verdict**: **reject**. GPL-2.0-or-later and LGPL-2.1 are out under the licence rule.

## podman (as a launcher)
- **Repo / licence (verified)**: https://github.com/podman-container-tools/podman (moved from containers/podman); Apache-2.0; `gh api` lic=`Apache-2.0`.
- **Form**: a daemonless CLI that drives conmon and an OCI runtime (crun or runc). Rootless; it uses `newuidmap` setuid helpers for subuid ranges. On macOS and Windows it runs `podman machine`, a VM through applehv, libkrun, WSL2 or Hyper-V.
- **Platforms**: Linux yes; macOS through a VM; Windows through a VM.
- **Mechanism**: an OCI container on Linux, or in the machine VM elsewhere.
- **Against our contract**:
  - Point 1 holds on Linux, run here. `podman --runtime runc run --network none -v sock:/run/memory --rootfs DIR` gave interfaces `['lo']`, TCP `ENETUNREACH` and `egress-ok` (`spike/podman.log`). On a Mac or Windows machine VM, a host socket under `-v` crosses through virtiofs or 9p and cannot be connected (see pattern B), so it would need pattern C, with the egress inside the machine, as wslc does.
  - Point 2 holds, run here: fixed paths, `ro` enforced.
- **Maintenance**: pushed 2026-10-19; v6.1.4 (2026-10-12); 33.1k stars. This machine has 4.9.3.
- **If we adopted it**: a second wslc-shaped backend for macOS (pattern C). On Linux it adds conmon, a runtime, subuid and networking packages for no isolation gain.
- **Verdict**: **learn-from**. On Linux it duplicates what we have; on Mac it is a heavier pattern C than Apple container.

## WSL containers (`wslc`), for reference
- **Repo / licence (verified)**: https://github.com/microsoft/WSL; MIT; `gh api` lic=`MIT`. `microsoft/wslc` does not exist (404). The wslc SDK is part of WSL.
- **Form**: a CLI plus a C API (`wslcsdk.dll`), on WSL's per-session VM.
- **Platforms**: Windows yes.
- **Mechanism**: containers in a WSL2 utility VM. We use pattern C: an egress container and a harness container with `--network none` share a volume that carries the socket (PLAN M6).
- **Against our contract**: points 1, 2 and 5 hold, as tested in M6.
- **Maintenance**: pushed 2026-10-19; release 3.0.3 (2026-10-09).
- **If we adopted it**: already adopted.
- **Verdict**: **keep**. microsandbox (WHP) is the only candidate here that could also serve Windows from one codebase.

## Others current in October 2026, briefly
- **Gondolin** (https://github.com/earendil-works/gondolin; Apache-2.0, `gh api`; pushed 2026-10-04; v0.13.0). A QEMU microVM (experimental `krun`) with a TypeScript control plane that owns network and filesystem through HTTP hooks. It needs Node on the host. The network model is "proxy every request in JS", which overlaps our egress rather than giving one socket. **Learn-from.**
- **vfkit** (https://github.com/crc-org/vfkit; Apache-2.0; pushed 2026-10-19; v0.6.4). A macOS Virtualization.framework CLI with `--device virtio-vsock` exposed as a host unix socket (`doc/usage.md:395-405`). It is a pattern-B building block on macOS used by podman machine. **Learn-from.**
- **Docker Sandboxes / `sbx`** (https://github.com/docker/sbx-releases; `gh api` NOASSERTION; `LICENSE` reads "Copyright © 2026 Docker Inc. All rights reserved."). **Reject**: proprietary.
- **Hyperlight** (https://github.com/hyperlight-dev/hyperlight; Apache-2.0; v0.18.0, 2026-10-14). A micro-VM for guest functions, not a Linux userland, so it cannot run Node CLIs. **Reject.**
- **crosvm** (https://github.com/google/crosvm; BSD-3-Clause, `gh api`). A Linux VMM with virtio-fs and vsock. Same role as Cloud Hypervisor. **Learn-from.**
- **gvisor-tap-vsock** (https://github.com/containers/gvisor-tap-vsock; Apache-2.0; v0.9.0). A user-mode network over vsock used by podman, Lima and BoxLite. Relevant only if a network is ever preferred to a socket. **Learn-from.**

---

## Ranked shortlist for this layer

1. **gVisor**: an optional hardened Linux backend. Rootless, no KVM, no daemon. Points 1 and 2 were shown here with an OCI spec generated the way our `Plan` would generate it. Linux only.
2. **microsandbox**: the only Apache-2.0 candidate whose documented features give "no NIC plus one host-socket route" and fixed-path read-only and read-write mounts on Linux, macOS and Windows from one OCI image. It is beta, and libkrunfw carries an LGPL/GPL source obligation.
3. **Apple container / Containerization**: the macOS boundary to try against the dry-coded Seatbelt. Its socket relay presents a unix socket, so `amem relay` does not change. Needs macOS 26 and Apple Silicon.
4. **libkrun (direct)**: the engine under 2 and most others. Fall back to it if microsandbox's state, API churn or agent are unwanted. It needs a binding crate and our own guest init.
5. **smolvm**: the alternative to 2 if its host-socket forwarding generalises. Its mediated-egress protocol is worth reading.

### Spike 1: gVisor, rootless (no root, no `/dev/kvm`, no podman, no Node)

Already run on this machine. The commands are below; `S` is the scratch directory.

```sh
S=$SCRATCH/research
mkdir -p $S/gvisor-bin/x && cd $S/gvisor-bin
gh release download release-20261012.0 -R google/gvisor -p gvisor-x86_64.tar.bz2 -p SHA256SUMS
grep x86_64.tar.bz2 SHA256SUMS | sha256sum -c
tar -xjf gvisor-x86_64.tar.bz2 -C x          # runsc plus gvisor-bin/ sidecars; all are needed
R=$S/gvisor-bin/x/runsc; $R --version         # release-20261012.0

# A host "egress": a unix socket that answers "egress-ok" (scripts/echo_server.py).
python3 -I $S/scripts/echo_server.py $S/spike/sock/egress.sock &   # run in the background

# Point 1 and point 2: an OCI bundle with a read-only root and fixed-path binds.
mkdir -p $S/spike/bundle && cd $S/spike/bundle && $R spec -- /usr/bin/python3 -I /work/probe.py
python3 -I $S/scripts/mkspec.py $S/spike/bundle $S/spike/rootfs2 $S/spike   # adds the mounts and a netns
$R --rootless --network=none --host-uds=open --ignore-cgroups --root=$S/spike/runsc-state run spike1
```

Result here (`spike/gvisor-run.log`): `unix: egress-ok`; `tcp: [Errno 101] Network is
unreachable`; interfaces `lo` only; `/work` writable, and the write was seen on the host;
`/opt/memory` `EROFS`; `/usr/bin` not writable.

Next steps:
- Point `root.path` at the unpacked harness image (`amem image unpack`), not a hand-made rootfs.
- Run `claude --version`, then a headless prompt, through `amem relay` inside, to test Node under
  the Sentry.
- Try `runsc --platform=kvm` once `/dev/kvm` is accessible.
- Gotcha: do not bind host `/usr` on WSL2, because of the locked submounts (`EINVAL`).

### Spike 2: microsandbox (needs `/dev/kvm`; no podman, no Node)

`/dev/kvm` here is `crw-rw---- root kvm`, and `striker` is not in group `kvm`
(`test -r /dev/kvm -a -w /dev/kvm` gave no access). The one root step is
`sudo usermod -aG kvm striker`, then log in again, or `sudo setfacl -m u:striker:rw /dev/kvm`.

```sh
S=$SCRATCH/research
D=$S/msb && mkdir -p $D/dl && cd $D/dl
gh release download v0.7.8 -R superradcompany/microsandbox -p microsandbox-linux-x86_64.tar.gz -p checksums.sha256
grep microsandbox-linux-x86_64.tar.gz checksums.sha256 | sha256sum -c
tar -xzf microsandbox-linux-x86_64.tar.gz
mkdir -p $D/bin $D/lib && install -m755 msb $D/bin/ && cp libkrunfw.so.* $D/lib/   # the layout install.sh uses
export MSB_HOME=$D                                     # install.sh's INSTALL_DIR; keeps state inside $D

python3 -I $S/scripts/echo_server.py $S/spike/sock/egress.sock &   # the host "egress"

# Point 2: fixed paths, read-only and read-write. Point 1: no network except vsock port 5000 to the host socket.
$D/bin/msb run --no-net --vsock $S/spike/sock/egress.sock:5000 \
  --mount-dir $S/spike/tree:/work --mount-dir $S/spike/home:/home/user \
  --mount-dir $S/spike/tree:/opt/memory:ro \
  python:3.12-slim -- python3 -c '
import socket
v = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM); v.connect((2, 5000)); print("vsock:", v.recv(64))
try: socket.create_connection(("1.1.1.1", 443), timeout=3); print("tcp: CONNECTED (bad)")
except OSError as e: print("tcp:", e)
import os; print(os.listdir("/sys/class/net")); open("/work/w.txt","w").write("x")
try: open("/opt/memory/x","w")
except OSError as e: print("ro:", e)'
```

What passes:
- `vsock: b'egress-ok\n'`.
- TCP refused.
- `w.txt` appears on the host.
- `ro: Read-only file system`.

Then:
- Repeat through the Rust SDK with `.network(|n| n.enabled(false))`. CLI `--no-net` keeps the
  device and denies by policy; the SDK form removes the NIC, which is the form point 1 wants.
- Boot the unpacked harness image as a rootfs directory, as `examples/rust/root-bind` does.
- Run Claude Code with `amem relay` taught to dial `AF_VSOCK` 2:5000.

The `msb` CLI flags above are from `docs/cli/sandbox-commands.mdx` and were not run here.
