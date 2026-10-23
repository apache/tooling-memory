#!/bin/sh
# microsandbox (superradcompany/microsandbox, Apache-2.0), the `msb` CLI: a libkrun microVM
# per run, no daemon, no root, but /dev/kvm. MSB names the binary, with libkrunfw in
# ../lib beside it as its installer lays it out; MSB_HOME holds its state and image cache.
#
# The egress crosses the VM boundary as a vsock port the host maps to its unix socket,
# so the check is told to dial vsock 2:5000 instead of a path. NET picks how the network
# is cut: `none` (the default) or any other `--net` profile, or a path to a `--net-conf`
# file. The fixture's directories go in over virtiofs at the Linux layout, presented as
# owned by uid 1000, the harness user. Arguments, if any, replace the check.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/fixture.sh"
set +e
[ $# -gt 0 ] || set -- python3 -I /work/contract-check.py
case ${NET:-none} in
    */*) net="--net-conf=$NET" ;;
    *) net="--net=${NET:-none}" ;;
esac
owner=uid=1000,gid=1000
"${MSB:?set MSB to the msb binary}" run --no-tty --no-stdin --replace --name "contract-$$" \
    "$net" \
    --vsock "$FIX/run/egress.sock:5000" \
    --mount-dir "$FIX/home:/home/user:$owner" \
    --mount-dir "$FIX/work:/work:$owner" \
    --mount-dir "$FIX/opt/memory:/opt/memory:ro,$owner" \
    --mount-dir "$FIX/projection:/memory:ro,$owner" \
    --user 1000:1000 --workdir /work \
    -e HOME=/home/user -e CHECK_SOCKET=vsock:2:5000 -e "CHECK_HOST_MARKER=$CHECK_HOST_MARKER" \
    "${IMAGE:-docker.io/library/python:3.12-slim}" -- "$@"
echo "exit=$?"
