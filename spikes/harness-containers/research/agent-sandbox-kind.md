# Spike B: kubernetes-sigs/agent-sandbox on kind, where the egress sidecar goes

Run 20 October 2026. Question: in the institutional shape (a harness pod with our egress as
a sidecar), which pod layout holds contract point 1, "the harness has no network except one
unix socket to the egress"?

Manifests, `run.sh` and a README are in
`$REPO/spikes/harness-containers/agent-sandbox/`
(uncommitted). The full output of the last `run.sh` run is beside this file:
`$SCRATCH/research/agent-sandbox-kind.run.log`.

## Setup (all from commands run here)

- Disk: `df -h /` at start `1007G size, 311G used, 645G avail`; at end `310G used, 646G avail`.
- kind v0.33.0 (`kind-linux-amd64.sha256sum`: OK), kubectl v1.37.1 (`kubectl.sha256`: OK), in
  `scratchpad/kind/bin`. Node image `kindest/node:v1.37.0`, containerd 2.3.4, kernel
  6.18.40.1-microsoft-standard-WSL2. Rootless podman 4.9.3, cgroup v2, user slice delegates
  `cpu memory pids`.
- **kind starts rootless here with cpu, memory, pids only.** The one failure was podman's log
  driver, not cgroups: the first `kind create cluster` ended
  `ERROR: failed to create cluster: failed to read logs: command "podman logs -f as-spike-control-plane" failed ... Error: using --follow with the journald --log-driver but without the journald --events-backend (file) is not supported`.
  `CONTAINERS_CONF_OVERRIDE` pointing at a file with `[containers] log_driver = "k8s-file"`
  fixed it, with no root step; the cluster came up and the node went Ready.
- agent-sandbox v1.0.7 (latest release, published 2026-10-15), `sandbox-with-extensions.yaml`
  sha256 `136c10c9...f2af0da8` (the release ships no checksum file). The controller rolled out;
  CRDs `Sandbox`, `SandboxTemplate`, `SandboxWarmPool`, `SandboxClaim`.
- Image `python:3.12-slim` loaded from the local podman store into the node
  (`podman save` + `kind load image-archive`; `kind load docker-image` reported
  "not present locally" under podman). `imagePullPolicy: Never`.
- Every harness container: uid 1000, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`,
  all capabilities dropped, no service-account token, `/home/user`, `/work`, `/tmp` emptyDirs,
  `/memory` an emptyDir mounted readOnly, `/opt/memory/memory` a ConfigMap (mode 0555), the check
  from a ConfigMap. The egress stand-in is python3 serving HTTP 200 on `/run/memory/egress.sock`
  in a shared emptyDir (A, B), or on TCP 8080 behind a Service (C).
- **kindnet enforces NetworkPolicy.** Its log shows `"Starting controller" name="kube-network-policies"`
  and `"Policy engine is ready."`, and layout C below shows connections blocked once the policy applies.

## A. One pod: egress sidecar + harness, check run directly

```
FAIL  1  only loopback  (eth0,lo)
FAIL  1  no TCP to the internet  (connected)
FAIL  1  no name resolution  (resolved)
PASS  1  no vsock socket  (Operation not permitted)
PASS  1  the egress socket answers  (HTTP/1.0 200 OK)
PASS  2  home writable at /home/user
PASS  2  work writable at /work
PASS  2  projection readable, not writable, at /memory  (Read-only file system)
PASS  2  system readable, not writable, at /usr  (Read-only file system)
PASS  2  memory binary present, read-only, at /opt/memory/memory
PASS  2  no read-write mount under /usr
exit=1
```

What the harness reaches (`probe.py`):

```
REACH    kube-api 10.96.0.1:443
REACH    kube-dns 10.96.0.10:53
REACH    other-pod-http 10.244.0.4:8080  (HTTP/1.0 404 Not Found)    # CoreDNS pod
REACH    internet 1.1.1.1:443
```

In an earlier `run.sh` run, the same harness also reached the separate egress pod of layout C
(`REACH other-pod-http 10.244.0.18:8080 (HTTP/1.0 200 OK)`). **Point 1 broken**: the harness has
the pod's interface, the internet, cluster DNS, the API server and other pods. Point 2 holds.
AF_VSOCK is refused by the RuntimeDefault seccomp profile.

## B. Same pod, harness runs our namespaces backend (`unshare`) before the check

Two commands per variant: `unshare -Urn` (user + net, as asked), and
`unshare -Urmnpf --mount-proc` with a tmpfs over `/sys` (user + mount + net + pid with a fresh
`/proc`, which is what `crates/sandbox/src/linux.rs` does; it leaves `/sys` out of its root).

| Variant | `unshare -Urn` | `unshare -Urmnpf --mount-proc` | Point 1 |
|---|---|---|---|
| RuntimeDefault (`layout-a`) | `unshare: unshare failed: Operation not permitted` | same | not reachable: refused |
| Unconfined (`b-unconfined`) | runs | runs | **FAIL** on vsock |
| Localhost `amem-userns.json` (`b-localhost`) | runs | runs | **all PASS** |
| `hostUsers: false` + RuntimeDefault (`b-userns`) | pod never starts | | not testable here |
| `hostUsers: false` + Unconfined + `procMount: Unmasked` (`b-userns-unconfined`) | pod never starts | | not testable here |

Unconfined, full shape:

```
PASS  1  only loopback  (lo)
PASS  1  no TCP to the internet  (Network is unreachable)
PASS  1  no name resolution  ([Errno -3] Temporary failure in name resolution)
FAIL  1  no vsock socket  (created)
PASS  1  the egress socket answers  (HTTP/1.0 200 OK)
PASS  2  (all six)
info  pid 1 is python3; 1 processes visible
exit=1
```

Localhost profile, full shape (`b-localhost`):

```
PASS  1  only loopback  (lo)
PASS  1  no TCP to the internet  (Network is unreachable)
PASS  1  no name resolution  ([Errno -3] Temporary failure in name resolution)
PASS  1  no vsock socket  (Operation not permitted)
PASS  1  the egress socket answers  (HTTP/1.0 200 OK)
PASS  2  home writable at /home/user
PASS  2  work writable at /work
PASS  2  projection readable, not writable, at /memory  (Read-only file system)
PASS  2  system readable, not writable, at /usr  (Read-only file system)
PASS  2  memory binary present, read-only, at /opt/memory/memory
PASS  2  no read-write mount under /usr
info  pid 1 is python3; 1 processes visible
exit=0
```

and from inside the same namespaces every target is `BLOCKED ... (Network is unreachable)`
(API server, cluster DNS, CoreDNS pod, 1.1.1.1).

Notes, each from a run:

- With `unshare -Urn` alone the check FAILs "only loopback (eth0,lo)" because `/sys/class/net` is
  the pod's sysfs, mounted for the pod's netns; `/proc/net/dev` in the same process shows only
  `lo`, and TCP and DNS fail. That FAIL is the check reading a stale sysfs, not a route out.
  Covering `/sys` (as our backend's root does) makes it PASS.
- Unconfined lets unshare through and also lets the harness create AF_VSOCK sockets, which on a
  VM-hosted node (WSL2 here, any cloud VM) reach the hypervisor host outside every namespace. Our
  backend installs no seccomp filter of its own (`grep -i seccomp crates/sandbox/src/*.rs`: no
  hits), so Unconfined is not acceptable.
- `amem-userns.json` is generated by `seccomp-userns.py` from moby's default profile (Apache-2.0,
  `docker-v29.9.1`, `vendor/github.com/moby/profiles/seccomp/default.json`): rules gated on a
  capability dropped (the harness holds none), then unshare, setns, clone, mount, umount2,
  pivot_root and the fsopen family allowed. socket() stays limited to the default's families, so
  AF_VSOCK (40) stays refused. A first version without the fsopen family let `unshare` and
  `--mount-proc` (mount(2)) work but `mount -t tmpfs` (util-linux tries fsopen first) failed
  `permission denied`; adding them fixed it.
- `hostUsers: false` fails on this node with
  `Failed to create pod sandbox: ... failed to create network namespace for sandbox "...": failed to set effective UID: invalid argument`.
  The kind node is itself a rootless user namespace with 65,537 ids (`/proc/self/uid_map` in the
  node: `0 1000 1` and `1 100000 65536`; host `/etc/subuid`: `striker:100000:65536`). The likely cause, inferred from
  that map and the error rather than traced in the kubelet's code: the pod's user namespace
  needs a range of ids the node does not have. The root step that would
  let it run: give the user a larger subordinate range in `/etc/subuid` and `/etc/subgid` (e.g.
  `usermod --add-subuids` / `--add-subgids` with a range of many times 65,536), `podman system migrate`,
  recreate the cluster; or run the kind node under rootful podman. Not attempted. Whether
  `hostUsers: false` alone admits `unshare` under RuntimeDefault was therefore not observed.

## C. Two pods: egress behind a Service, harness under deny-all except the egress

Before the policy:

```
REACH    egress-svc-http 10.96.66.180:8080  (HTTP/1.0 200 OK)
REACH    egress-pod-http 10.244.0.26:8080  (HTTP/1.0 200 OK)
REACH    kube-api 10.96.0.1:443
REACH    kube-dns 10.96.0.10:53
REACH    other-pod-http 10.244.0.4:8080  (HTTP/1.0 404 Not Found)
REACH    internet 1.1.1.1:443
```

With `harness-only-egress` (Ingress+Egress, egress only TCP 8080 to `role=egress`):

```
REACH    egress-svc-http 10.96.66.180:8080  (HTTP/1.0 200 OK)
REACH    egress-pod-http 10.244.0.26:8080  (HTTP/1.0 200 OK)
BLOCKED  kube-api 10.96.0.1:443  (timed out)
BLOCKED  kube-dns 10.96.0.10:53  (timed out)
BLOCKED  other-pod-http 10.244.0.4:8080  (timed out)
BLOCKED  internet 1.1.1.1:443  (timed out)
```

Contract check (no socket in this layout; the route out is TCP):

```
FAIL  1  only loopback  (eth0,lo)
PASS  1  no TCP to the internet  (timed out)
PASS  1  no name resolution  ([Errno -3] Temporary failure in name resolution)
PASS  1  no vsock socket  (Operation not permitted)
FAIL  1  the egress socket answers  (No such file or directory)
PASS  2  (all six)
exit=1
```

**Point 1 held in effect, not in form**: the harness keeps an interface and reaches the egress
over pod-network TCP; the isolation lives in the CNI's policy engine, outside the pod, and is only
as good as that engine (kindnet's kube-network-policies here; a CNI without one enforces nothing).
The egress Service is reachable from any pod that no policy restricts (layout A's harness reached
it), so the egress would also need an ingress policy or caller authentication to know whose
session a request is.

## Warm pool shape (B as a template)

`template.yaml`: a `SandboxTemplate` with the `b-localhost` pod, a `SandboxWarmPool` of 2, a
`SandboxClaim` `session-1`. After apply: three Sandboxes, two owned by `SandboxWarmPool`, one
owned by `SandboxClaim` (in the interactive run their creation times suggest the claim adopted a
warm one and the pool refilled; not checked further). The controller created
`harness-network-policy` for the template by default: Egress to `0.0.0.0/0` except
`10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16` (and IPv6 ULA/link-local), Ingress
only from `app: sandbox-router` in `agent-sandbox-system`. In the claimed pod: the egress sidecar
reached 1.1.1.1:443, timed out to the API server, and resolved `api.anthropic.com` (why DNS
passes that policy was not investigated); the harness, under the full unshare, passed every
contract check (`exit=0`, same lines as `b-localhost`).

## Conclusion

The institutional shape should use **layout B: the egress as a native sidecar in the harness pod,
the socket in a shared emptyDir, and our namespaces backend run inside the harness container
under a Localhost seccomp profile that is the runtime default plus user namespaces and mounting**
(`amem-userns.json`). It is the only layout where the check passed every line, point 1 in its
literal form (only `lo`, nothing reachable, one unix socket that answers) and point 2. It is the
laptop backend unchanged inside a pod, so there is one confinement code path, not a Kubernetes
variant, and it stamps out through `SandboxTemplate`/`SandboxWarmPool`/`SandboxClaim`, with the
template's managed NetworkPolicy keeping the sidecar off cluster-internal addresses.

Why not the others: A gives the harness the pod's whole network. B under RuntimeDefault refuses
`unshare`; B under Unconfined runs but opens AF_VSOCK, a route out on any VM-hosted node.
`hostUsers: false` could not start on this rootless node, so it is unproven here; it would not
remove the need for a seccomp profile that admits `unshare` unless shown otherwise. C holds only by
policy enforced outside the pod, changes the contract from a unix socket to pod-network TCP, and
needs the egress to authenticate callers. Cost of B: a custom seccomp profile on every node (the
Security Profiles Operator or the node image), which is a cluster-admin step, as installing
agent-sandbox's CRDs already is.

## Cleanup (from commands run)

`kind delete cluster --name as-spike` (node and its volume removed; no new podman volume left
against the start-of-run list), `podman rmi` of the `kindest/node` image (24f646398ba8), and
`podman network rm kind` (created by this task's first, pre-wipe run). `python:3.12-slim` left in
place. `podman ps -a --external` shows no container of this task.
