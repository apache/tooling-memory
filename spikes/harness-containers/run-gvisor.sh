#!/bin/sh
# gVisor (google/gvisor, Apache-2.0): `runsc --rootless --network=none --host-uds=open`,
# no daemon, no KVM (the systrap platform). RUNSC names the binary, with its sidecars
# beside it as the release tarball unpacks them. The root is an image's filesystem, not
# the host's /usr: binding the host's /usr fails rootless on WSL2, where its submounts
# are locked. ROOTFS names an unpacked rootfs with python3; without it, one is exported
# from IMAGE through podman into the fixture. Arguments, if any, replace the check as the
# command run inside. RUNSC_FLAGS adds flags to runsc, such as
# `--file-access-mounts=exclusive`.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/fixture.sh"
trap fixture_down EXIT
set +e
runsc=${RUNSC:?set RUNSC to the runsc binary}
rootfs=${ROOTFS:-}
if [ -z "$rootfs" ]; then
    rootfs="$FIX/rootfs"
    mkdir -p "$rootfs"
    cid=$(podman create -q "${IMAGE:-docker.io/library/python:3.12-slim}")
    podman export "$cid" | tar -x -C "$rootfs" 2>/dev/null
    podman rm -f "$cid" >/dev/null
fi
mkdir -p "$FIX/bundle" "$FIX/state"
[ $# -gt 0 ] || set -- python3 -I /work/contract-check.py
(cd "$FIX/bundle" && "$runsc" spec -- "$@")
python3 -I - "$FIX/bundle/config.json" "$rootfs" "$FIX" "$CHECK_HOST_MARKER" <<'EOF'
import json, sys
path, rootfs, fix, marker = sys.argv[1:]
c = json.load(open(path))
c["root"] = {"path": rootfs, "readonly": True}
c["process"].update(terminal=False, cwd="/work", user={"uid": 0, "gid": 0})
c["process"]["env"] += ["HOME=/home/user", "CHECK_HOST_MARKER=" + marker]
def bind(src, dst, ro):
    return {"destination": dst, "type": "bind", "source": src,
            "options": ["rbind"] + (["ro"] if ro else [])}
c["mounts"] += [
    bind(fix + "/home", "/home/user", False),
    bind(fix + "/work", "/work", False),
    bind(fix + "/opt/memory", "/opt/memory", True),
    bind(fix + "/projection", "/memory", True),
    bind(fix + "/run", "/run/memory", False),
    {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev"]},
]
# Rootless runsc maps the caller to root inside and refuses a mapping of its own (with no
# user namespace in the spec it says so; with one, it cannot fork its gofer), so the
# workload runs as uid 0, which is the caller outside, and not as the layout's uid 1000.
json.dump(c, open(path, "w"), indent=1)
EOF
# shellcheck disable=SC2086 # RUNSC_FLAGS is a list of flags
"$runsc" --rootless --network=none --host-uds=open --ignore-cgroups --root="$FIX/state" $RUNSC_FLAGS \
    run --bundle "$FIX/bundle" "contract-$$"
echo "exit=$?"
