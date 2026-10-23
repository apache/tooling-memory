#!/bin/sh
# ferroday-cage (MIT OR Apache-2.0), its CLI `fcage` 0.5.0 from crates.io. FCAGE names the
# binary. The seccomp rules refuse AF_VSOCK (40) and io_uring, as in run-nsjail.sh;
# FCAGE_SECCOMP= runs without them.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/fixture.sh"
set +e
mkdir -p "$FIX/root"
(cd "$FIX/root" && ln -s usr/bin bin && ln -s usr/lib lib && ln -s usr/lib64 lib64 && ln -s usr/sbin sbin)
if [ -n "${FCAGE_SECCOMP+set}" ]; then
    set --
else
    set -- --seccomp-deny-rule 'socket arg=0 len=dword op=eq value=40' --seccomp-deny io_uring_setup
fi
"${FCAGE:?set FCAGE to the fcage binary}" --rootfs "$FIX/root" \
    --ro-bind /usr /usr --ro-bind /etc /etc \
    --bind "$FIX/home" /home/user --bind "$FIX/work" /work \
    --ro-bind "$FIX/opt/memory" /opt/memory --ro-bind "$FIX/projection" /memory \
    --bind "$FIX/run" /run/memory \
    --setenv HOME /home/user --setenv CHECK_HOST_MARKER "$CHECK_HOST_MARKER" --chdir /work \
    "$@" \
    -- /usr/bin/python3 -I /work/contract-check.py
echo "exit=$?"
