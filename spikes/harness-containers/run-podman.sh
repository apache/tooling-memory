#!/bin/sh
# Rootless podman, its default OCI runtime, `--network none`, the fixture bind-mounted at
# the Linux layout. RUNTIME=runsc (or any OCI runtime podman knows) swaps the runtime
# under the same flags; IMAGE swaps the image, which only needs python3, and ROOTFS runs
# an unpacked rootfs instead of an image. Arguments, if any, replace the check as the
# command run inside.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/fixture.sh"
set +e
[ $# -gt 0 ] || set -- python3 -I /work/contract-check.py
if [ -n "${ROOTFS:-}" ]; then
    set -- --rootfs "$ROOTFS" "$@"
else
    set -- "${IMAGE:-docker.io/library/python:3.12-slim}" "$@"
fi
# the caller is uid 1000 inside whatever it is outside, so the fixture it owns is writable
podman run --rm --network none --userns keep-id:uid=1000,gid=1000 --user 1000:1000 --read-only \
    ${RUNTIME:+--runtime "$RUNTIME"} \
    -v "$FIX/home:/home/user:rw" \
    -v "$FIX/work:/work:rw" \
    -v "$FIX/opt/memory:/opt/memory:ro" \
    -v "$FIX/projection:/memory:ro" \
    -v "$FIX/run:/run/memory:rw" \
    --tmpfs /tmp:rw,mode=1777 -e CHECK_HOST_MARKER -e HOME=/home/user \
    -w /work \
    "$@"
echo "exit=$?"
