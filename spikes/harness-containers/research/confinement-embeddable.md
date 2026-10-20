# Embeddable confinement: libraries and single binaries in place of crates/sandbox

Researched 19 October 2026. Every licence below was read from the repository or crate
manifest in this session; the command is given with each. Repo metadata comes from
`gh api repos/OWNER/REPO --jq '{full_name,pushed_at,archived,stargazers_count,license:.license.spdx_id}'`,
and crate data from `https://crates.io/api/v1/crates/<name>`.

## What the agent harnesses ship (October 2026)

| Harness | Linux | macOS | Windows | Source |
| ------- | ----- | ----- | ------- | ------ |
| Codex CLI | bubblewrap (vendored C, compiled into a `bwrap` binary by `codex-rs/bwrap`, or a system `bwrap` on PATH) + in-process seccomp; Landlock kept as a legacy path | Seatbelt, profiles in `codex-rs/sandboxing/src/*.sbpl` | own `codex-windows-sandbox` (sandbox users, ACLs, WFP, elevated one-time setup), and since this autumn MXC `BaseContainerRunner` behind `features.prefer_mxc` | `codex-rs/linux-sandbox/README.md`, `codex-rs/mxc-sandbox/README.md` |
| Claude Code (`srt`) | exec of system `bwrap` + `socat`, prebuilt seccomp BPF | `sandbox-exec` | `srt-win.exe` (Rust): dedicated local user, WFP filter on its SID, ACEs | `anthropics/sandbox-runtime` README |
| Gemini CLI | bwrap args builder (`packages/core/src/sandbox/linux/bwrapArgsBuilder.ts`); also docker/podman/runsc/lxc | Seatbelt `.sb` profiles | C# helper: restricted token, Low integrity, job object | `google-gemini/gemini-cli` tree |
| Copilot CLI | Bubblewrap, opt-in `/sandbox enable`, preview | Seatbelt | ProcessContainer (Insiders); reported to run on MXC | docs.github.com "about cloud and local sandboxes"; secondary reports for MXC |
| Cursor CLI | Landlock + seccomp directly, kernel 6.2+, userns; closed source | Seatbelt | inside WSL2 | cursor.com/blog/agent-sandboxing, cursor.com/docs/agent/tools/terminal |
| Goose | none found in `aaif-goose/goose` (tree grep for sandbox/seatbelt/landlock/bwrap: no hits) | Desktop only: `sandbox-exec` + local CONNECT proxy | none | goose-docs.ai/docs/guides/sandbox |

The Linux pattern is overwhelmingly "exec bubblewrap". Nobody in this set links a
permissive namespace library; Cursor and nono go Landlock-first. Codex's and srt's
network design is the same as ours: `--unshare-net` plus a TCP-to-UDS relay inside.

**Finding against our own backend (measured here):** `unshare -Urn python3 -I vsock.py`
on this WSL2 kernel 6.18 creates an `AF_VSOCK` socket inside a fresh network namespace
and its `connect((2, 1))` to the host CID times out (errno 110) rather than failing with
ENETUNREACH: the network namespace does not cover vsock. Codex's
`linux-sandbox/src/seccomp.rs` and srt's `vendor/seccomp-src/seccomp-unix-block.c` both
block `AF_VSOCK` socket creation and `io_uring_*` for exactly this reason.
`grep -rniE 'no_new_privs|seccomp|vsock' crates/` in our repo finds no seccomp filter and
no `PR_SET_NO_NEW_PRIVS` in `crates/sandbox`. Contract point 1 ("NO network") therefore
has a hole on VM hosts (WSL2, any guest with a vsock device) that a small seccomp filter
closes. This is the most valuable thing this layer offers us.

---

## OpenAI Codex sandbox crates (codex-rs)

- **Repo / licence (verified)**: https://github.com/openai/codex, `Apache-2.0`.
  `gh api repos/openai/codex/license` -> Apache-2.0; `codex-rs/Cargo.toml`
  `[workspace.package] license = "Apache-2.0"`, every sandbox crate uses
  `license.workspace = true`. Exception inside the tree: `codex-rs/vendor/bubblewrap/`
  is upstream bubblewrap (`COPYING` = GNU Library GPL v2; upstream `bubblewrap.c` says
  `SPDX-License-Identifier: LGPL-2.0-or-later`), compiled by `codex-rs/bwrap/build.rs`
  with `cc` into a `bwrap` binary linked against libcap.
- **Form**: Rust crates in a monorepo, plus helper binaries (`codex-linux-sandbox`,
  `bwrap`, `codex-windows-sandbox-setup`, `codex-command-runner`). Not published:
  crates.io returns not-found for `codex-linux-sandbox`, `codex-sandboxing`,
  `codex-windows-sandbox`, `codex-process-hardening`, `codex-network-proxy`; workspace
  version is `0.0.0`. Vendorable only by copying source with attribution (NOTICE file
  at repo root). Unprivileged on Linux/macOS; Windows needs a one-time elevated setup
  (sandbox users, firewall).
- **Platforms**: Linux yes; macOS yes; Windows yes (own backend + MXC).
- **Mechanism**: Linux: bubblewrap builds the mount/user/pid/net namespaces
  (`--ro-bind / /`, writable roots via `--bind`), then the helper sets
  `PR_SET_NO_NEW_PRIVS` and a seccompiler filter (blocks network syscalls, `AF_VSOCK`,
  `io_uring`, ptrace). In proxy mode, `--unshare-net` plus an internal TCP->UDS->TCP
  bridge, and seccomp forbids new standalone `AF_UNIX` sockets after the bridge is up.
  macOS: generated Seatbelt profile. Windows: sandbox user + ACLs + WFP, or MXC.
- **Against our contract**: 1 holds (same design as ours: netns + UDS bridge; adds
  vsock/io_uring blocking we lack). 2 breaks: the filesystem model is the host's own
  paths (`--ro-bind / /` then overlays); no fixed in-box layout like `/work`, `/home/user`;
  bwrap could do it but codex's policy layer does not express it. 3 breaks: it is
  bubblewrap (LGPL) underneath. 4 holds (Seatbelt). 5 different approach (native
  Windows, not WSL containers). 6 silent. 7: we would not be adding `unsafe` if we
  only borrowed the seccomp rule set via `seccompiler`.
- **Maintenance**: pushed 2026-10-19, latest release `rust-v0.163.0` 2026-10-15, not
  archived, 129,931 stars.
- **If we adopted it**: cannot cleanly adopt: unpublished, tied to `codex-protocol`
  and its permission-profile types, and the Linux path is bwrap. Borrowing
  `linux-sandbox/src/seccomp.rs`'s rule set (Apache-2.0, with NOTICE) adds
  `seccompiler` (Apache-2.0 OR BSD-3-Clause) and nothing else.
- **Verdict**: **borrow**. Copy the seccomp rule set (AF_VSOCK, io_uring, AF_UNIX after
  the relay is up, ptrace) and the Seatbelt base policy as references for our dry-coded
  macOS backend; do not adopt the crates.

## Anthropic sandbox-runtime (`srt`)

- **Repo / licence (verified)**: https://github.com/anthropics/sandbox-runtime (the
  old `anthropic-experimental/sandbox-runtime` redirects there), `Apache-2.0`.
  `gh api repos/anthropic-experimental/sandbox-runtime` -> full_name
  `anthropics/sandbox-runtime`, license Apache-2.0; `package.json` license
  `Apache-2.0`, name `@anthropic-ai/sandbox-runtime`, version 0.0.80.
- **Form**: Node/TypeScript library and CLI (`srt`, `dist/cli.js`); npm deps
  `@pondwader/socks5-server`, `commander`, `node-forge`, `zod`. On Linux it **spawns**
  separately installed `bwrap` and `socat` (`spawn(socat, ...)`, `spawnSync(bwrap, ...)`
  in `src/sandbox/linux-sandbox-utils.ts`; paths overridable via `bwrapPath` /
  `socatPath`), plus `rg` for deny scanning. It does not link or bundle bubblewrap
  (LGPL-2.0+) or socat (GPL-2.0); exec of a separately installed binary is an
  arm's-length use, not a derivative work, but it is still a runtime dependency we would
  have to ask users to install. Seccomp: prebuilt BPF for x86-64/arm64 plus a small C
  helper `apply-seccomp.c` from the repo. Windows: bundled `srt-win.exe` (Rust, in
  `vendor/srt-win-src`), needs a one-time elevated `windows-install`.
- **Platforms**: Linux yes (with bwrap, socat, rg installed); macOS yes; Windows alpha.
- **Mechanism**: Linux: bwrap with `--unshare-net`, host proxies listening on unix
  sockets bind-mounted in, `socat` inside relaying a loopback TCP port to them, then
  seccomp blocking new `AF_UNIX`/`AF_VSOCK` sockets. macOS: generated Seatbelt profile
  allowing only one localhost port. Windows: dedicated `srt-sandbox` user, WFP fence.
- **Against our contract**: 1 holds in shape (netns + bind-mounted UDS + in-box relay is
  exactly ours, with socat where we have `amem relay`) but srt also starts its own
  HTTP/SOCKS proxies on the host, which duplicate crates/egress. 2 breaks: read is
  allow-by-default over the host (`--ro-bind / /`), write allow-listed, paths stay at
  host spellings; no private home, no fixed `/work`. 3 breaks (bubblewrap + socat). 4
  holds. 5 different approach. 6 overlaps and conflicts (its proxies vs ours). 7 n/a
  (Node).
- **Maintenance**: pushed 2026-10-19, latest release v0.0.80 2026-10-16, not archived,
  5,596 stars. Pre-1.0.
- **If we adopted it**: adds Node to a Rust binary's launch path, bwrap, socat and rg
  as host prerequisites, and a second proxy layer. Replaces little we do not already have.
- **Verdict**: **learn-from**. Its README is the most careful public write-up of the
  edge cases (mandatory deny list for `.git/hooks`, `.bashrc`, symlink-in-path masking,
  WSL `/mnt/c` mount points); read it as a test checklist for our plan.

## Microsoft eXecution Container (MXC)

- **Repo / licence (verified)**: https://github.com/microsoft/mxc, `MIT`.
  `gh api repos/microsoft/mxc/license` -> MIT; `LICENSE.md` is the MIT text, (c)
  Microsoft Corporation; crates.io `mxc-sdk` 1.0.1 license MIT.
- **Form**: Rust library (`mxc-sdk`, builds engine and selected backends into the
  consuming app), also .NET and Node SDKs and executor binaries (`wxc-exec.exe`,
  `lxc-exec`). No daemon for process backends; the WSLC backend has an optional
  `wxc-wslc-daemon` for state-aware containers. Unprivileged on Linux/macOS; Windows
  ProcessContainer needs Windows 11 24H2+ servicing builds, no UAC per launch.
- **Platforms**: Linux yes (default backend `bubblewrap`, also `lxc`, `microvm`,
  `hyperlight`); macOS yes (`seatbelt`); Windows yes (`processcontainer` default,
  `wslc`, `windows_sandbox`/`microvm`/`hyperlight` experimental, `isolation_session`).
- **Mechanism**: one JSON request (filesystem RO/RW/denied path lists, directional
  network policy, proxy URL) mapped to a backend. Linux: exec of system `bwrap`
  ("Bubblewrap installed and on PATH", `docs/backends/bwrap/bubblewrap-backend.md`);
  `egress.default: "deny"` is `--unshare-net` alone, proxy modes add `slirp4netns`,
  `ip`, `unshare`, iptables. Windows ProcessContainer: per-container SID, WFP egress
  filters, per-container WinHTTP proxy. WSLC: links `wslcsdk.dll` via the `wslc`
  feature.
- **Against our contract**: 1: Linux deny mode is netns-only; there is no first-class
  "one unix socket in" grant, but a socket path listed in `readwritePaths` would be
  bound at the same path. 2 breaks: the schema (`Filesystem` = `readonlyPaths`,
  `readwritePaths`, `deniedPaths`) has no source-to-target mapping; every path appears at
  its host spelling, and WSLC maps `C:\x` to `/mnt/c/x`. 3 breaks (bwrap). 4 holds
  (Seatbelt, AF_UNIX governed by path). 5: overlaps directly with crates/wslc-api, but
  the WSLC backend documents no stdin ("The WSLC SDK exposes no process-input API"), no
  in-container mount targets, and cooperative-only proxying; our binding already does
  more. 6 silent (it expects a proxy URL, which our egress is not). 7: it is a dependency,
  so its `unsafe` is not in our crates.
- **Maintenance**: pushed 2026-10-19, v1.0.1 2026-10-17 (v1.0.0 2026-10-07), not
  archived, 2,925 stars. Adopted by Codex (`codex-rs/mxc-sandbox`).
- **If we adopted it**: on Windows, a native (non-WSL) backend for free: ProcessContainer
  via a maintained MIT crate. On Linux it would swap our namespaces code for a bwrap
  dependency, which is a regression against point 3. Adds the `windows` crate stack,
  flatbuffers, tracelogging.
- **Verdict**: **learn-from now, revisit for Windows**. Not a Linux replacement. If we
  ever want a native-Windows backend beside `wslc`, `mxc-sdk` with ProcessContainer is the
  first thing to try; it cannot give fixed paths, so the plan would have to accept host
  spellings there.

## nono (nolabs-ai)

- **Repo / licence (verified)**: https://github.com/nolabs-ai/nono (formerly
  `always-further/nono`, `lukehinds/nono`, both redirect), `Apache-2.0`.
  `gh api repos/always-further/nono` -> license Apache-2.0; crates.io `nono` 0.80.0
  license Apache-2.0; `crates/nono/Cargo.toml` `license.workspace = true`.
- **Form**: Rust library crate `nono` plus CLI (`nono-cli`) and `nono-proxy`; Python
  bindings. Default feature `system-keyring` (turn off). Unprivileged.
- **Platforms**: Linux yes; macOS yes; Windows only via WSL2.
- **Mechanism**: Landlock (filesystem, TCP ports, abstract-unix and signal scopes) and
  seccomp fallback on Linux, Seatbelt on macOS. Opt-in `linux_namespace.rs`: user + net +
  ipc + uts namespaces (no mount namespace), a loopback-only netns, and an optional
  loopback TCP listener inside it handed to the supervisor outside.
- **Against our contract**: 1 holds (netns with loopback, supervisor-served listener: a
  close cousin of our relay). 2 breaks: no mount namespace, so no private home, no fixed
  paths; Landlock grants host paths in place. 3 holds the "no bwrap, no daemon" part.
  4 holds. 5 n/a. 6 overlaps (credential-injecting proxy). 7 n/a.
- **Maintenance**: pushed 2026-10-19, v0.80.0 2026-10-13, 4,579 stars, frequent releases.
- **If we adopted it**: would add Landlock as a second layer, but cannot replace the mount
  plan. Pulls `sigstore-verify`, `x509-cert`, `keyring` (optional), `typify` build dep.
- **Verdict**: **borrow** (ideas: Landlock layering over our mounts, the netns loopback
  listener handoff). Not a replacement, because no mount namespace.

## nsjail (Google)

- **Repo / licence (verified)**: https://github.com/google/nsjail, `Apache-2.0`
  (`gh api repos/google/nsjail/license`). Links libprotobuf (BSD-3) and, only if
  `pkg-config --exists libnl-route-3.0` at build time, libnl-3 (LGPL-2.1); the Makefile
  makes it optional (`NL3_EXISTS`), so build without it. Kafel (seccomp policy language)
  is a submodule built into `kafel/libkafel.a`.
- **Form**: single C++ binary, exec'd. Unprivileged with user namespaces.
- **Platforms**: Linux yes; macOS no; Windows no.
- **Mechanism**: clone with user/mount/net/pid/ipc/uts/cgroup namespaces, chroot or
  pivot_root into a directory, `-R src:dst` / `-B src:dst` bind mounts, `-T` tmpfs,
  rlimits, cgroups, Kafel seccomp policies, optional pasta networking.
- **Against our contract**: 1 holds: network namespace by default (`-N` disables it),
  and a bind of the egress socket file to `/run/memory/egress.sock` is an ordinary
  `-B src:dst`. 2 holds: arbitrary binds at fixed targets (`-R /usr:/usr`,
  `-B $HOME_DIR:/home/user`, `-B $TREE:/work`, `-R $MEM:/opt/memory/memory`). 3 holds
  (no daemon, no bwrap) but is a second binary to ship. 4/5 no. 6 silent. 7 holds (no
  `unsafe` of ours), and the in-box seccomp gap closes with `--seccomp_string`.
- **Maintenance**: pushed 2026-10-02, release 3.6 2026-03-18, 4,163 stars, not archived.
- **If we adopted it**: replaces `crates/sandbox/src/linux.rs` (~370 lines) with a
  command-line generator, adds a C++ binary plus libprotobuf (and its abseil deps) to
  ship or build per distro. Adds Kafel seccomp, rlimits, cgroups for free.
- **Verdict**: **learn-from / fallback**. The only permissive single binary here that
  holds points 1 and 2 as stated; not worth the shipping cost while our own namespaces
  code works, but the obvious fallback if it ever does not. Spike 1 below ran here and
  held points 1 and 2. Build from master: release 3.6 still requires libnl-3 (LGPL-2.1).

## minijail (Google / ChromeOS)

- **Repo / licence (verified)**: https://github.com/google/minijail, `BSD-3-Clause`
  (`gh api repos/google/minijail/license`). Rust bindings in-repo: `rust/minijail`
  (0.2.3) and `rust/minijail-sys` (0.0.14, bindgen + pkg-config); neither is on
  crates.io (`minijail` not found).
- **Form**: C library `libminijail` + `minijail0` binary; Rust safe wrapper over it.
- **Platforms**: Linux yes (Android/ChromeOS); macOS no; Windows no.
- **Mechanism**: `-U` user ns, `-v` mount ns, `-e` net ns, `-b src,dst,writable` binds,
  `-P` pivot_root, `-S` seccomp policy file, `--fs-path-*` Landlock rules.
- **Against our contract**: 1 holds (net ns + bind of the socket). 2 holds (`-b src,dst`).
  3 holds. 7: the Rust wrapper is safe but `minijail-sys` is FFI; we would depend on it,
  not write the `unsafe` ourselves, but would build a C library from source.
- **Maintenance**: last commit 2026-10-16; tags are dated (`linux-v2026.05.18`); last
  GitHub "release" object is linux-v18 from 2023. Active, ChromeOS-paced.
- **If we adopted it**: a C build dependency and a vendored, unpublished crate pair.
- **Verdict**: **learn-from**. Same capability as our code, worse packaging for us.

## landlock crate (rust-landlock)

- **Repo / licence (verified)**: https://github.com/landlock-lsm/rust-landlock; GitHub
  reports `NOASSERTION` because it is dual: repo holds `LICENSE-APACHE` and
  `LICENSE-MIT`, `Cargo.toml` `license = "MIT OR Apache-2.0"`; crates.io `landlock`
  0.4.7 says the same.
- **Form**: Rust library, safe API; unprivileged.
- **Platforms**: Linux yes (kernel 5.13+, features by ABI); macOS no; Windows no.
- **Mechanism**: restricts the calling thread and its descendants to path-beneath and
  TCP-port rules, plus (newer ABIs) abstract-unix and signal scopes.
- **Against our contract**: not a replacement: it cannot remap paths (2) and cannot
  isolate a network namespace (1); it is defence in depth inside our box.
- **Maintenance**: pushed 2026-08-04, v0.4.7 2026-07-27, 18.8 M downloads.
- **If we adopted it**: one small pure-Rust dependency; used by Codex, nono, extrasafe,
  hakoniwa, ferroday-cage.
- **Verdict**: **adopt as an add-on** (after the pivot, restrict writes to `/work`,
  `/home/user`, `/tmp`; deny abstract unix sockets), alongside a seccomp filter.

## seccompiler (rust-vmm)

- **Repo / licence (verified)**: crates.io `seccompiler` 0.5.0, license
  `Apache-2.0 OR BSD-3-Clause`, repo rust-vmm/seccompiler (Firecracker lineage).
- **Form**: pure-Rust BPF compiler for seccomp filters; no libseccomp.
- **Platforms**: Linux only.
- **Mechanism**: builds and installs a seccomp-bpf program from Rust rule maps.
- **Against our contract**: closes the vsock/io_uring gap in point 1; nothing else.
- **Maintenance**: 0.5.0 published 2025-03-07, 25 M downloads; used by Codex and extrasafe.
- **If we adopted it**: one dependency; installing the filter is an `unsafe`-free call in
  the crate, made in `crates/sandbox` before exec.
- **Verdict**: **adopt** (with Codex's rule set as the reference).

## ferroday-cage

- **Repo / licence (verified)**: https://github.com/gregordinary/ferroday-cage;
  GitHub `Apache-2.0`, repo holds `LICENSE-APACHE` and `LICENSE-MIT`, crates.io
  `ferroday-cage` 0.4.5 and `ferroday-cage-cli` 0.5.0 license `MIT OR Apache-2.0`.
- **Form**: pure-Rust library with a typed builder, plus `fcage` CLI. No C deps by
  default; `subid` feature execs `newuidmap`. Unprivileged, no daemon.
- **Platforms**: Linux yes (5.6+; overlay needs 5.11/6.6); macOS no; Windows no.
- **Mechanism**: user + mount + pid (+ net) namespaces, pivot_root into a rootfs
  directory, `Bind::new(source, target).read_only(..)`, tmpfs/overlay mounts, minimal
  init as pid 1, optional `hardening` (Landlock, seccomp, cap drops), optional
  in-process userspace TCP/IP stack (`netstack`).
- **Against our contract**: 1 holds: `Network::Isolated` is the default, "a new network
  namespace whose only interface is loopback" (`spec.rs`), and a socket file binds like
  any other path. 2 holds: binds take an explicit target. 3 holds, and is the closest
  thing to our own code as a library. 7: its `unsafe` lives in three named modules of
  its own; we would write none.
- **Maintenance**: created 2026-07-26, last push 2026-09-26, tag v0.4.5; one contributor
  (41 commits), 2 stars, ~400 downloads. Pre-1.0, API "settles over the 0.x series".
- **If we adopted it**: would replace `linux.rs` with a dependency whose bus factor is
  one and whose age is three months; adds a rootfs-directory requirement (we already
  pivot into the unpacked image, so this fits).
- **Verdict**: **learn-from, watch**. Right shape, too young to put under a boundary.
  Spike 2 below ran here and held points 1 and 2.

## hakoniwa

- **Repo / licence (verified)**: https://github.com/souk4711/hakoniwa; GitHub
  `NOASSERTION`; `LICENSE.md`: library `LGPL-3.0-only WITH LGPL-3.0-linking-exception`,
  CLI `GPL-3.0-only`; crates.io `hakoniwa` 1.8.0 license
  `LGPL-3.0-only WITH LGPL-3.0-linking-exception`.
- **Form**: Rust library + CLI; namespaces, Landlock, seccomp (via `libseccomp`, C),
  cgroups via youki's `libcgroups`, pasta networking.
- **Platforms**: Linux only.
- **Maintenance**: pushed 2026-10-06, v1.8.0 2026-09-25, 89 stars.
- **Against our contract / If we adopted it**: functionally close to ours, but the
  licence is out.
- **Verdict**: **reject** (LGPL-3.0, even with the linking exception: weak copyleft
  outside Category A).

## birdcage (Phylum)

- **Repo / licence (verified)**: https://github.com/phylum-dev/birdcage, `GPL-3.0`
  (`gh api repos/phylum-dev/birdcage/license`); crates.io 0.8.1 `GPL-3.0-or-later`.
- **Maintenance**: **archived**, last push 2026-07-06, last crate 2024-04-19.
- **Verdict**: **reject** (GPL, archived).

## extrasafe

- **Repo / licence (verified)**: https://github.com/boustrophedon/extrasafe, `MIT`
  (GitHub and `Cargo.toml`).
- **Form**: Rust library: seccomp (via seccompiler 0.4), optional Landlock, `isolate`
  feature for user-namespace isolation. Unprivileged.
- **Platforms**: Linux only.
- **Against our contract**: the `isolate` module can build a namespace jail, but it is
  aimed at self-sandboxing a Rust process, not launching a Node harness with a mount plan.
- **Maintenance**: last push 2024-07-13, last crate 0.5.1 2024-04-16. Stale.
- **Verdict**: **learn-from** (its seccomp rule groupings), not adopt: stale.

## gaol (Servo)

- **Repo / licence (verified)**: https://github.com/servo/gaol, `Apache-2.0` on GitHub,
  crates.io 0.2.1 `MIT/Apache-2.0`.
- **Form**: Rust library; Linux namespaces + seccomp, macOS Seatbelt, FreeBSD.
- **Maintenance**: last push 2024-12-03, last crate 2019-10-16.
- **Verdict**: **reject** (unmaintained; profile model is per-path allowlist, no fixed
  target binds).

## youki `libcontainer`

- **Repo / licence (verified)**: https://github.com/youki-dev/youki, `Apache-2.0`;
  crates.io `libcontainer` 0.7.0 `Apache-2.0`. Transitive deps checked:
  `pathrs` is `MPL-2.0 OR LGPL-3.0-or-later` (Category B at best), default feature
  `libseccomp` links the C libseccomp (LGPL-2.1) through `libseccomp` crate (MIT OR
  Apache-2.0), `rust-criu` MIT, `oci-spec`/`libcgroups`/`nc`/`safe-path` Apache-2.0.
- **Form**: Rust library implementing an OCI runtime; usable without a daemon, rootless
  supported. Takes an OCI bundle (rootfs + `config.json`).
- **Platforms**: Linux only.
- **Mechanism**: full OCI runtime: namespaces, mounts from the spec, cgroups (systemd or
  v2), seccomp from the spec, capabilities.
- **Against our contract**: 1 and 2 hold (OCI spec mounts with destinations, empty netns),
  3 holds formally (library, no daemon) but brings a container runtime's surface and
  cgroup/systemd assumptions. 7 n/a.
- **Maintenance**: pushed 2026-10-19, v0.7.0 2026-07-25, 7,641 stars.
- **If we adopted it**: large dependency tree, an MPL-2.0 dependency in `pathrs`, and
  libseccomp unless the default feature is turned off.
- **Verdict**: **reject** for this layer (weight and `pathrs` licence), though the OCI
  config shape is a useful reference if our plan ever serialises.

## landrun

- **Repo / licence (verified)**: https://github.com/Zouuup/landrun, `MIT`.
- **Form**: single Go binary; Landlock only. **Platforms**: Linux only.
- **Against our contract**: no namespaces: breaks 1 (only TCP-port rules) and 2.
- **Maintenance**: pushed 2026-10-16, v0.1.17 2026-07-22, 2,318 stars.
- **Verdict**: **reject** for this layer.

## Windows-native options

- **Codex `codex-windows-sandbox`** (Apache-2.0, above): sandbox users, ACLs, WFP,
  elevated setup; unpublished. **learn-from**.
- **srt-win** (`anthropics/sandbox-runtime/vendor/srt-win-src`, Apache-2.0): dedicated
  local user + WFP on its SID; elevated one-time install. **learn-from**.
- **MXC ProcessContainer** (MIT, above): the maintained crate; Windows 11 24H2 servicing
  builds. **revisit** if a native Windows backend is wanted.
- **rappct** (https://github.com/cpjet64/rappct; GitHub `NOASSERTION`, crates.io 0.13.3
  `MIT`): Rust AppContainer helper; 7 stars, last release 2025-10-23. **reject**
  (too small to trust under a boundary).
- **isobox** (https://github.com/can1357/isobox, `MIT`, Go): Seatbelt / gVisor / AppContainer
  behind one CLI; 28 stars, v0.1.0 2026-06-05. **learn-from** only.
- **Gemini CLI `GeminiSandbox.cs`** (Apache-2.0): restricted token, Low integrity, job
  object. **learn-from**.

None of these give a fixed in-box path layout; Windows-native confinement works on host
paths and ACLs. Our `wslc` backend is the only route to contract point 2 on Windows.

## Out on licence

- **bubblewrap**: https://github.com/containers/bubblewrap, GitHub `NOASSERTION`;
  `COPYING` is LGPL 2.1 text; `bubblewrap.c` header `SPDX-License-Identifier:
  LGPL-2.0-or-later`. v0.13.1 2026-10-14. Out (LGPL). Codex compiles it in; srt, MXC,
  Gemini CLI and Copilot exec it.
- **firejail**: https://github.com/netblue30/firejail, `GPL-2.0`. Out.
- **socat** (srt's Linux relay): GPL-2.0. Out; we have `amem relay`.
- **syd / sydbox**: crates.io `syd` 3.59.0 `GPL-3.0-only`. Out.
- **crun**: https://github.com/containers/crun, `GPL-2.0` (library parts LGPL). Out.

---

## Ranked shortlist for this layer

1. **Keep `crates/sandbox/src/linux.rs`, add `seccompiler` + `landlock` (adopt)**, with
   Codex's `linux-sandbox/src/seccomp.rs` as the reference rule set (borrow, Apache-2.0,
   attribute via NOTICE). Closes the measured `AF_VSOCK` hole and `io_uring`, adds
   `PR_SET_NO_NEW_PRIVS`; both crates are pure Rust and permissively licensed.
2. **nsjail** (Apache-2.0, single binary): the only permissive exec-able tool here that
   holds points 1 and 2 as written. Fallback if we ever stop owning namespaces code.
3. **ferroday-cage** (MIT OR Apache-2.0, pure-Rust library): the right shape as a library
   replacement; watch until it has more than one maintainer and a 1.0.
4. **MXC `mxc-sdk`** (MIT): for a future native-Windows backend (ProcessContainer) and as a
   cross-check of our Seatbelt generator; not for Linux (bwrap) and not instead of our
   `wslc-api` (no stdin, no mount targets).
5. **nono** (Apache-2.0): Landlock-first library; borrow the netns-listener handoff and
   Landlock layering, cannot replace the mount plan.

Both spikes below were run on this machine and held points 1 and 2 with the same
in-box result; in both, `AF_VSOCK` was creatable until a one-line seccomp rule was
added, which is the same gap our own backend has.

Both spikes below were run on this machine and held points 1 and 2 with the same
in-box result; in both, `AF_VSOCK` was creatable until a one-line seccomp rule was
added, which is the same gap our own backend has.

## Spike 1: nsjail

No root, no /dev/kvm, no Node. Uses podman to build (the host has no protoc, bison,
flex). Install directory: `$SP/nsjail`.

```sh
SP=$SCRATCH/research/spike
mkdir -p $SP/nsjail
podman run --rm -v $SP/nsjail:/out:Z docker.io/library/ubuntu:24.04 bash -c '
  set -e
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    git g++ make pkg-config bison flex protobuf-compiler libprotobuf-dev
  git clone -q --depth 1 --recurse-submodules https://github.com/google/nsjail /src
  cd /src && make -j8
  cp nsjail /out/
  for l in $(ldd nsjail | awk "/protobuf|absl/{print \$3}"); do cp -L $l /out/; done'
export LD_LIBRARY_PATH=$SP/nsjail
```

Build from master (built here at commit `4ff54a6e`, 2026-10-02), not the 3.6 tag: at 3.6
`net.cc` includes `<netlink/route/link.h>` unconditionally, so the release needs libnl-3
(LGPL-2.1); the build here failed on exactly that. Master builds without it. Result:
`nsjail` linked only against libprotobuf.so.32, libstdc++, libc, libz, libgcc_s, libm.

Contract test (points 1 and 2), on the host, reusing the box from spike 2's setup
(`$SP/box/root` with the usr-merge symlinks, `scripts/echo_server.py` serving
`$SP/box/egress.sock`):

```sh
(cd $SP/box && rm -f egress.sock && python3 -I $SC/echo_server.py egress.sock &)
$SP/nsjail/nsjail -Mo -Q --chroot $SP/box/root \
  -R /usr:/usr -R /etc:/etc \
  -B $SP/box/home:/home/user -B $SP/box/work:/work \
  -R $SP/box/opt:/opt/memory \
  -R $SP/box/egress.sock:/run/memory/egress.sock \
  -R $SC/inbox.py:/opt/test/inbox.py \
  -T /tmp --cwd /work -E HOME=/home/user \
  --rlimit_as inf --time_limit 0 \
  --seccomp_string 'ERRNO(1) { socket(domain, type, protocol) { domain == 40 }, io_uring_setup } DEFAULT ALLOW' \
  -- /usr/bin/python3 -I /opt/test/inbox.py
```

**Result, run here 19 October 2026 (exit 0):**

```text
root: ['bin', 'dev', 'etc', 'home', 'lib', 'lib64', 'opt', 'proc', 'run', 'sbin', 'tmp', 'usr', 'work']
net/dev: ['lo']
TCP out: [Errno 101] Network is unreachable
AF_VSOCK socket: [Errno 1] Operation not permitted     # without --seccomp_string: "created"
unix socket: pong from host egress
write /work ok
write /home/user ok
write /usr -> Read-only file system
write /opt/memory -> Read-only file system
home exists from host? False
```

Points 1 and 2 hold, identically to spike 2. nsjail's defaults (`--rlimit_as` 4096 MB and
`--time_limit` 600 s, per `nsjail --help`) must be lifted for a long-running Node harness,
as above.

## Spike 2: ferroday-cage

No root, no /dev/kvm, no podman, no Node; needs cargo. Install directory: `$SP/fcage`.

```sh
SP=$SCRATCH/research/spike
SC=$SCRATCH/research/scripts
CARGO_TARGET_DIR=$SP/fcage/target cargo install --locked ferroday-cage-cli --version 0.5.0 --root $SP/fcage
# (0.4.5 is the library's version; the CLI crate is at 0.5.0. Built in 2m43s here.)

# an empty root with the usr-merge symlinks, a private home, a tree, a stand-in egress socket
mkdir -p $SP/box/root $SP/box/home $SP/box/work $SP/box/opt
(cd $SP/box/root && ln -sfn usr/bin bin && ln -sfn usr/lib lib && ln -sfn usr/lib64 lib64 && ln -sfn usr/sbin sbin)
# relative bind: the absolute scratchpad path exceeds the 108-byte sun_path limit
(cd $SP/box && python3 -I $SC/echo_server.py egress.sock &)

$SP/fcage/bin/fcage --rootfs $SP/box/root \
  --ro-bind /usr /usr --ro-bind /etc /etc \
  --bind $SP/box/home /home/user --bind $SP/box/work /work \
  --ro-bind $SP/box/opt /opt/memory \
  --ro-bind $SP/box/egress.sock /run/memory/egress.sock \
  --ro-bind $SC/inbox.py /opt/test/inbox.py \
  --setenv HOME /home/user --chdir /work \
  --seccomp-deny-rule 'socket arg=0 len=dword op=eq value=40' --seccomp-deny io_uring_setup \
  -- /usr/bin/python3 -I /opt/test/inbox.py
```

`scripts/echo_server.py` answers one line on a unix socket; `scripts/inbox.py` lists `/`,
the interfaces in `/proc/net/dev`, tries TCP to 1.1.1.1:443, creates an `AF_VSOCK`
socket, talks to `/run/memory/egress.sock`, and tries a write in each mount.

**Result, run here 19 October 2026 (exit 0):**

```text
root: ['bin', 'dev', 'etc', 'home', 'lib', 'lib64', 'opt', 'proc', 'run', 'sbin', 'tmp', 'usr', 'work']
net/dev: ['lo']
TCP out: [Errno 101] Network is unreachable
AF_VSOCK socket: [Errno 1] Operation not permitted     # without the seccomp rule: "created"
unix socket: pong from host egress
write /work ok
write /home/user ok
write /usr -> Read-only file system
write /opt/memory -> Read-only file system
home exists from host? False
```

Points 1 and 2 hold: loopback only, the one socket reachable at a fixed in-box path,
arbitrary binds at fixed targets, nothing else of the host visible. Without the explicit
seccomp rule `AF_VSOCK` sockets are still creatable, as in our backend.
