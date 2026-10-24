#!/bin/sh
# Our Seatbelt backend on macOS: `amem launch --harness command` running the check, with the
# real egress amem starts. Seatbelt keeps every host path, so the check is told where each
# thing is: the home and the projection from the plan's environment, the egress through the
# relay's loopback port (MEMORY_MCP_URL), the working tree and the memory binary from here.
# A marker in the real home must stay unreadable, and a listener the host opens on loopback
# unreachable. On Linux the same script runs our namespaces backend, which checks the
# script. AMEM names the binary; PYTHON the interpreter inside, /usr/bin/python3 by default,
# which the profile's system paths cover.
here=$(cd "$(dirname "$0")" && pwd)
amem=$(realpath "${AMEM:-$here/../../target/debug/amem}")
python=${PYTHON:-/usr/bin/python3}
. "$here/fixture.sh"
set +e
# short, because the egress socket's path under it must fit a unix socket address (104
# bytes on macOS)
root=$(mktemp -d /tmp/am.XXXXXX)
marker="$HOME/.contract-check-marker.$$"
listener=
cleanup() {
    [ -z "$listener" ] || kill "$listener" 2>/dev/null
    rm -rf "$root" "$root.port" "$marker"
    fixture_down
}
trap cleanup EXIT
echo secret > "$marker"
python3 -I -c '
import socket, sys
s = socket.create_server(("127.0.0.1", 0))
print(s.getsockname()[1], flush=True)
while True:
    s.accept()[0].close()
' > "$root.port" &
listener=$!
i=0
while [ ! -s "$root.port" ] && [ $i -lt 50 ]; do i=$((i + 1)); sleep 0.1; done
port=$(cat "$root.port")
work=$(realpath "$FIX/work")
memory_bin=
[ "$(uname)" = Darwin ] && memory_bin="export CHECK_MEMORY_BIN='$amem'"
cat > "$work/check-inside.sh" <<EOF
relay=\${MEMORY_MCP_URL%/mcp}
export CHECK_HOME="\$HOME" CHECK_PROJECTION="\${MEMORY_PROJECTION:-/memory}"
export CHECK_SOCKET="tcp:\${relay#http://}" CHECK_WORK='$work'
export CHECK_HOST_MARKER='$marker' CHECK_LOOPBACK_OTHER='$port'
$memory_bin
exec '$python' -I '$work/contract-check.py'
EOF
"$amem" --root "$root" init --owner human:spike:me --domain dom-spike >/dev/null
OPENAI_API_KEY=sk-placeholder "$amem" --root "$root" launch --harness command --auth key \
    --workdir "$work" -- sh "$work/check-inside.sh" </dev/null
echo "exit=$?"
