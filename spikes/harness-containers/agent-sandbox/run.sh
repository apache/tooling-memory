#!/bin/sh
# Contract point 1 for a harness pod with the egress beside it, under
# kubernetes-sigs/agent-sandbox on kind (rootless podman by default). Creates the cluster if it is
# missing, installs agent-sandbox, applies each layout, and runs ../contract-check.py in
# the harness container of each. DOWN=1 deletes the cluster at the end. ENGINE=docker
# runs the node under Docker instead: rootful, as on a CI runner, which
# `hostUsers: false` needs (idmapped mounts).
#
# Needs kind, kubectl, the engine, gh and python3 on PATH, and the image
# docker.io/library/python:3.12-slim in the engine's store (it is loaded into the node, so
# the cluster pulls nothing but its own images).
set -u
here=$(cd "$(dirname "$0")" && pwd)
CLUSTER=${CLUSTER:-as-spike}
AS_VERSION=${AS_VERSION:-v1.0.7}
MOBY_TAG=${MOBY_TAG:-docker-v29.9.1}
IMAGE=docker.io/library/python:3.12-slim
NODE=$CLUSTER-control-plane
NS=as-spike
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

ENGINE=${ENGINE:-podman}
if [ "$ENGINE" = podman ]; then
    export KIND_EXPERIMENTAL_PROVIDER=podman
    # kind follows the node's log with `podman logs -f`, which podman refuses when its log
    # driver is journald and its event backend is not; k8s-file avoids that.
    printf '[containers]\nlog_driver = "k8s-file"\n' > "$work/containers.conf"
    export CONTAINERS_CONF_OVERRIDE="$work/containers.conf"
fi
k() { kubectl --context "kind-$CLUSTER" "$@"; }
x() { pod=$1; shift; k -n $NS exec "$pod" -c harness -- "$@"; }
say() { printf '\n### %s\n' "$*"; }

if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    kind create cluster --name "$CLUSTER" || exit 1
fi
k wait --for=condition=Ready node --all --timeout=180s

say "agent-sandbox $AS_VERSION"
gh release download "$AS_VERSION" -R kubernetes-sigs/agent-sandbox \
    -p sandbox-with-extensions.yaml -D "$work" || exit 1
sha256sum "$work/sandbox-with-extensions.yaml"
k apply --server-side -f "$work/sandbox-with-extensions.yaml" >/dev/null || exit 1
k -n agent-sandbox-system rollout status deploy/agent-sandbox-controller --timeout=300s

say "image and seccomp profile into the node"
"$ENGINE" save -o "$work/image.tar" "$IMAGE" >/dev/null && kind load image-archive "$work/image.tar" --name "$CLUSTER"
gh api "repos/moby/moby/contents/vendor/github.com/moby/profiles/seccomp/default.json?ref=$MOBY_TAG" \
    -H 'Accept: application/vnd.github.raw' > "$work/default.json" || exit 1
python3 -I "$here/seccomp-userns.py" "$work/default.json" > "$work/amem-userns.json" || exit 1
"$ENGINE" exec "$NODE" mkdir -p /var/lib/kubelet/seccomp/profiles
"$ENGINE" cp "$work/amem-userns.json" "$NODE:/var/lib/kubelet/seccomp/profiles/amem-userns.json"

say "kindnet: is there a NetworkPolicy engine?"
k -n kube-system logs ds/kindnet | grep -m1 'kube-network-policies' || echo "none found in kindnet's log"

k apply -f "$here/common.yaml" >/dev/null
k -n $NS create configmap contract-check --from-file=contract-check.py="$here/../contract-check.py" \
    --from-file=probe.py="$here/probe.py" --dry-run=client -o yaml | k apply -f - >/dev/null
k apply -f "$here/layout-a.yaml" -f "$here/layout-b.yaml" -f "$here/layout-c.yaml" >/dev/null
k -n $NS wait --for=condition=Ready sandbox/layout-a sandbox/b-unconfined sandbox/b-localhost \
    sandbox/c-egress sandbox/c-harness --timeout=180s

check='python3 -I /opt/check/contract-check.py'
# Without a mount namespace /sys still shows the pod's interfaces, so the netns-only run
# also prints /proc/net/dev, which follows the caller's network namespace.
netns_only="unshare -Urn sh -c 'cat /proc/net/dev; exec $check'"
# Our backend's shape: user+mount+net+pid with a fresh /proc; /sys covered by a tmpfs, as
# our backend leaves it out of the new root.
full="unshare -Urmnpf --mount-proc sh -c 'mount -t tmpfs none /sys && exec $check'"

say "A: harness and egress in one pod, harness runs the check directly"
x layout-a sh -c "$check"; echo "exit=$?"
svc=$(k -n $NS get svc c-egress -o jsonpath='{.spec.clusterIP}')
dns=$(k -n kube-system get svc kube-dns -o jsonpath='{.spec.clusterIP}')
egress_pod=$(k -n $NS get pod c-egress -o jsonpath='{.status.podIP}')
# another workload's pod: CoreDNS answers HTTP on 8080 (its health endpoint)
other=$(k -n kube-system get pod -l k8s-app=kube-dns -o jsonpath='{.items[0].status.podIP}')
targets="kube-api=10.96.0.1:443 kube-dns=$dns:53 other-pod-http=$other:8080 internet=1.1.1.1:443"
say "A: what the harness reaches"
x layout-a python3 -I /opt/check/probe.py $targets

for pod in layout-a b-unconfined b-localhost; do
    profile=$(k -n $NS get pod $pod -o jsonpath='{.spec.containers[0].securityContext.seccompProfile}')
    say "B: $pod $profile: unshare -Urn"
    x $pod sh -c "$netns_only"; echo "exit=$?"
    say "B: $pod $profile: unshare -Urmnpf --mount-proc"
    x $pod sh -c "$full"; echo "exit=$?"
done
say "B: b-localhost: what the harness reaches from inside its namespaces"
x b-localhost unshare -Urn python3 -I /opt/check/probe.py $targets
for pod in b-userns b-userns-unconfined; do
    say "B: $pod (hostUsers: false)"
    if k -n $NS wait --for=condition=Ready pod/$pod --timeout=60s 2>/dev/null; then
        x $pod sh -c "$full"; echo "exit=$?"
    else
        k -n $NS get events --field-selector involvedObject.name=$pod,reason=FailedCreatePodSandBox \
            -o jsonpath='{.items[-1:].message}{"\n"}'
        "$ENGINE" exec "$NODE" cat /proc/self/uid_map
    fi
done

ctargets="egress-svc-http=$svc:8080 egress-pod-http=$egress_pod:8080 $targets"
say "C: separate egress pod, no policy yet"
x c-harness python3 -I /opt/check/probe.py $ctargets
k apply -f "$here/layout-c-policy.yaml" >/dev/null
sleep 5
say "C: with harness-only-egress applied"
x c-harness python3 -I /opt/check/probe.py $ctargets
x c-harness sh -c "$check"; echo "exit=$?"

say "Warm pool: SandboxTemplate + SandboxWarmPool + SandboxClaim"
k apply -f "$here/template.yaml" >/dev/null
k -n $NS wait --for=condition=Ready sandboxclaim/session-1 --timeout=180s
claimed=$(k -n $NS get sandboxclaim session-1 -o jsonpath='{.status.sandbox.name}')
k -n $NS get sandbox -o custom-columns=NAME:.metadata.name,OWNER:.metadata.ownerReferences[*].kind | grep -E 'NAME|harness-'
k -n $NS get networkpolicy
x "$claimed" sh -c "$full"; echo "exit=$?"

if [ "${DOWN:-0}" = 1 ]; then
    kind delete cluster --name "$CLUSTER"
fi
