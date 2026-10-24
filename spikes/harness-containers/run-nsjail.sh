#!/bin/sh
# nsjail (google/nsjail, Apache-2.0), built from master: the 3.6 release needs libnl-3,
# which is LGPL. NSJAIL names the binary. The seccomp rule refuses AF_VSOCK (40) and
# io_uring, which a network namespace does not cover; NSJAIL_SECCOMP= runs without it.
# The limits nsjail sets by default (address space, ten minutes) are lifted, as a harness
# would need.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/fixture.sh"
set +e
mkdir -p "$FIX/root"
(cd "$FIX/root" && ln -s usr/bin bin && ln -s usr/lib lib && ln -s usr/lib64 lib64 && ln -s usr/sbin sbin)
if [ -n "${NSJAIL_SECCOMP+set}" ]; then
    set --
else
    set -- --seccomp_string 'ERRNO(1) { socket(domain, type, protocol) { domain == 40 }, io_uring_setup } DEFAULT ALLOW'
fi
"${NSJAIL:?set NSJAIL to the nsjail binary}" -Mo -Q --chroot "$FIX/root" \
    -R /usr:/usr -R /etc:/etc \
    -B "$FIX/home:/home/user" -B "$FIX/work:/work" \
    -R "$FIX/opt/memory:/opt/memory" -R "$FIX/projection:/memory" \
    -B "$FIX/run:/run/memory" \
    -T /tmp --cwd /work -E HOME=/home/user -E "CHECK_HOST_MARKER=$CHECK_HOST_MARKER" \
    --rlimit_as inf --time_limit 0 \
    "$@" \
    -- /usr/bin/python3 -I /work/contract-check.py
echo "exit=$?"
