#!/bin/sh
# macOS only: which permission our Seatbelt profile lacked for the relay, the first thing
# `amem launch` runs inside it. Kept as the record of how the missing grant was found: it
# rebuilds the profile as it was before 609304d, which added it. Rebuilds the profile's shape (crates/sandbox/src/seatbelt.rs:
# the header, the four services, the devices, the system read-only, the metadata of each
# directory above a grant, the memory binary read-only, a working directory read-write,
# loopback on the relay's port), then runs `amem relay ... -- /usr/bin/true` under it, and
# under it plus one class of permission at a time. The shell's status says how each ended:
# 128 + n is signal n. AMEM names the binary.
here=$(cd "$(dirname "$0")" && pwd)
amem=$(realpath "${AMEM:-$here/../../target/debug/amem}")
work=$(realpath "$(mktemp -d "${TMPDIR:-/tmp}/bisect.XXXXXX")")
trap 'rm -rf "$work"' EXIT
port=41999

above() {
    p=$(dirname "$1")
    while [ "$p" != / ]; do
        printf '    (literal "%s")\n' "$p"
        p=$(dirname "$p")
    done
}
ours() {
    cat <<EOF
(version 1)
(deny default)
(allow process-fork)
(allow process-exec)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow pseudo-tty)
(allow mach-lookup
    (global-name "com.apple.system.opendirectoryd.libinfo")
    (global-name "com.apple.system.logger")
    (global-name "com.apple.system.notification_center")
    (global-name "com.apple.trustd.agent"))
(allow file-read* file-write* file-ioctl
    (literal "/dev/null") (literal "/dev/zero") (literal "/dev/tty") (literal "/dev/ptmx")
    (subpath "/dev/fd") (regex #"^/dev/ttys[0-9]+$"))
(allow file-read* (literal "/dev/random") (literal "/dev/urandom"))
(allow file-read*
    (subpath "/usr") (subpath "/bin") (subpath "/sbin") (subpath "/System")
    (subpath "/Library") (subpath "/private/etc") (subpath "/private/var/db/dyld")
    (subpath "/private/var/db/timezone"))
(allow file-read-metadata
    (literal "/") (literal "/etc") (literal "/var") (literal "/tmp")
$(above "$amem")
$(above "$work/x"))
(allow file-read* (literal "$amem"))
(allow file-read* file-write* (subpath "$work"))
(allow network-outbound (remote unix-socket (path-literal "$work/egress.sock")))
(allow network-bind network-inbound (local ip "localhost:$port"))
(allow network-outbound (remote ip "localhost:$port"))
EOF
}
try() {
    name=$1
    profile=$2
    shift 2
    (cd "$work" && /usr/bin/sandbox-exec -p "$profile" "$@") > "$work/.out" 2>&1 </dev/null
    code=$?
    if [ $code -gt 128 ]; then how="signal $((code - 128))"; else how="exit $code"; fi
    printf 'BISECT  %-34s %s\n' "$name" "$how"
    sed 's/^/        /' "$work/.out" | head -8
}
relay() { echo "$amem" relay --socket "$work/egress.sock" --listen "127.0.0.1:$port" -- /usr/bin/true; }

base=$(ours)
echo "the profile under test:"
echo "$base" | sed 's/^/    /'
# shellcheck disable=SC2046 # relay's words are paths without spaces on a runner
try "allow default, relay" "(version 1)(allow default)" $(relay)
try "ours, /usr/bin/true alone" "$base" /usr/bin/true
try "ours, amem --version" "$base" "$amem" --version
# shellcheck disable=SC2046
try "ours, relay" "$base" $(relay)
# The first run (23 October): only a global `(allow file-read*)` let the relay start; of the
# Sandbox reports, the only ones naming its processes were `deny(1) file-read-data /` and
# `deny(1) file-write-data /dev/dtracehelper`. Each alone, then both:
root='(allow file-read-data (literal "/"))'
dtrace='(allow file-write-data (literal "/dev/dtracehelper"))'
# shellcheck disable=SC2046
try "ours + read of /" "$base
$root" $(relay)
# shellcheck disable=SC2046
try "ours + dtracehelper" "$base
$dtrace" $(relay)
# shellcheck disable=SC2046
try "ours + both" "$base
$root
$dtrace" $(relay)
try "ours + both, relay running sh" "$base
$root
$dtrace" "$amem" relay --socket "$work/egress.sock" --listen "127.0.0.1:$port" -- /bin/sh -c 'echo sh ran'
