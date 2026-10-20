#!/bin/sh
# The host side of a spike: a scratch tree laid out as the launcher lays out a session,
# and a stand-in egress answering HTTP on a unix socket: a 401 in the Anthropic API's shape,
# so a harness that reaches it fails at once and says why. Sourced by each run-*.sh, which
# then mounts these into its candidate box at the Linux layout and runs contract-check.py.
#
#   FIX/home        private home              -> /home/user          rw
#   FIX/work        working tree              -> /work               rw (holds the check)
#   FIX/opt/memory  the memory binary         -> /opt/memory         ro
#   FIX/projection  the record's projection   -> /memory             ro
#   FIX/run         the egress socket         -> /run/memory         (socket)
#   FIX/host-home   a host home with a marker, never mounted
#
# The caller sets `here` to this directory first. Leaves FIX and EGRESS_PID set;
# `fixture_down` stops the stand-in.
set -eu

FIX=$(mktemp -d "${TMPDIR:-/tmp}/contract.XXXXXX")
mkdir -p "$FIX/home" "$FIX/work" "$FIX/opt/memory" "$FIX/projection" "$FIX/run" "$FIX/host-home"
cp "$here/contract-check.py" "$here/harness-smoke.sh" "$here/bench.sh" "$FIX/work/"
printf '#!/bin/sh\necho memory\n' > "$FIX/opt/memory/memory"
chmod 0555 "$FIX/opt/memory/memory"
echo "# projection" > "$FIX/projection/README.md"
echo secret > "$FIX/host-home/marker"
CHECK_HOST_MARKER="$FIX/host-home/marker"
export CHECK_HOST_MARKER

python3 -I - "$FIX/run/egress.sock" <<'EOF' &
import socketserver, sys
class H(socketserver.StreamRequestHandler):
    def handle(self):
        # read the whole request, so a client still sending its body is not reset
        length = 0
        for line in iter(self.rfile.readline, b""):
            if line in (b"\r\n", b"\n"):
                break
            name, _, value = line.partition(b":")
            if name.strip().lower() == b"content-length":
                length = int(value)
        self.rfile.read(length)
        body = b'{"type":"error","error":{"type":"authentication_error","message":"stand-in egress"}}'
        self.wfile.write(b"HTTP/1.0 401 Unauthorized\r\nContent-Type: application/json\r\n"
                         b"Content-Length: %d\r\n\r\n%s" % (len(body), body))
socketserver.ThreadingUnixStreamServer(sys.argv[1], H).serve_forever()
EOF
EGRESS_PID=$!
i=0
while [ ! -S "$FIX/run/egress.sock" ] && [ $i -lt 50 ]; do i=$((i + 1)); sleep 0.1; done

fixture_down() {
    kill "$EGRESS_PID" 2>/dev/null || true
    chmod -R u+w "$FIX" 2>/dev/null || true
    rm -rf "$FIX"
}
