# agent-sandbox on kind: where the egress sidecar goes

Which pod layout holds contract point 1 (the harness has no network except one unix
socket to the egress) when a harness runs under
[kubernetes-sigs/agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox) with our
egress beside it. Containers in a pod share one network namespace, so the egress sidecar's
network is the harness's network unless something separates them.

| File | What it is |
|---|---|
| `run.sh` | creates a kind cluster on rootless podman if missing, installs agent-sandbox (release manifests), applies every layout, runs `../contract-check.py` in each harness; `DOWN=1` deletes the cluster |
| `common.yaml` | namespace, a stand-in egress (HTTP on a unix socket and/or TCP), a stand-in memory binary |
| `layout-a.yaml` | A: egress as a native sidecar and the harness in one Sandbox, the socket in a shared `emptyDir` |
| `layout-b.yaml` | B: the pod of A, varying the harness's seccomp profile and `hostUsers`, so our namespaces backend can `unshare` inside it |
| `layout-c.yaml`, `layout-c-policy.yaml` | C: the egress in its own Sandbox behind a Service; the harness pod under a deny-all NetworkPolicy except TCP to the egress |
| `seccomp-userns.py` | writes a Localhost seccomp profile: moby's default profile flattened for a container with no capabilities, plus unshare, setns, namespace flags on clone, and mounting |
| `probe.py` | which addresses a container can open TCP to |
| `template.yaml` | the B layout as a `SandboxTemplate`, a `SandboxWarmPool` of two, and a `SandboxClaim` |

The harness container runs `sleep infinity` and `run.sh` execs the check into it. In B the
check runs under `unshare -Urn` (user and net namespaces) and under
`unshare -Urmnpf --mount-proc` with a tmpfs over `/sys` (user, mount, net and pid
namespaces with a fresh `/proc`, the shape of `crates/sandbox/src/linux.rs`).

## Getting the tools

kind and kubectl are release binaries, put in an empty directory of their own:

```sh
curl -fsSLO https://github.com/kubernetes-sigs/kind/releases/download/v0.33.0/kind-linux-amd64
curl -fsSLO https://github.com/kubernetes-sigs/kind/releases/download/v0.33.0/kind-linux-amd64.sha256sum
sha256sum -c kind-linux-amd64.sha256sum && install -m755 kind-linux-amd64 kind
curl -fsSLO https://dl.k8s.io/release/v1.37.1/bin/linux/amd64/kubectl
curl -fsSLO https://dl.k8s.io/release/v1.37.1/bin/linux/amd64/kubectl.sha256
echo "$(cat kubectl.sha256)  kubectl" | sha256sum -c && chmod +x kubectl
```

`run.sh` sets `CONTAINERS_CONF_OVERRIDE` to a file with `log_driver = "k8s-file"`: with
podman's journald log driver and file event backend, `kind create cluster` fails on
`podman logs -f`. It loads `python:3.12-slim` from podman's store into the node, fetches
moby's default seccomp profile with `gh`, and copies the generated profile into the node's
`/var/lib/kubelet/seccomp/profiles/`. A real cluster distributes that profile with the
Security Profiles Operator or the node image.

## Cleaning up

`kind delete cluster --name as-spike` removes the node and its volume. The
`kindest/node` image and the `kind` podman network stay behind:
remove the image by its ID from `podman images | grep kindest` (it is pulled by digest,
so it has no tag) and run `podman network rm kind`.
