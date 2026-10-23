#!/bin/sh
# The host side of a spike: a scratch tree laid out as the launcher lays out a session,
# and a stand-in egress answering HTTP on a unix socket: a 401 in the Anthropic API's shape,
# so a harness that reaches it fails at once and says why. Sourced by each run-*.sh, which
# then mounts these into its candidate box at the Linux layout and runs contract-check.py.
#
# For a real call: UPSTREAM (such as https://api.anthropic.com) makes the stand-in forward
# each request there and stream the answer back, logging one line per request to stderr;
# LOGIN names a Claude Code `.credentials.json` whose access token, without its refresh
# token, is copied into the home, so nothing in the box can rotate the host's login; and
# CLAUDE_BIN names a Claude Code binary copied into the working tree, for an image that
# has none.
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
cp "$here/contract-check.py" "$here/harness-smoke.sh" "$here/harness-call.sh" "$here/bench.sh" "$FIX/work/"
if [ -n "${LOGIN:-}" ]; then
    mkdir -p "$FIX/home/.claude"
    python3 -I - "$LOGIN" > "$FIX/home/.claude/.credentials.json" <<'EOF'
import json, sys
o = json.load(open(sys.argv[1]))["claudeAiOauth"]
keep = ("accessToken", "expiresAt", "scopes", "subscriptionType", "rateLimitTier")
json.dump({"claudeAiOauth": {k: o[k] for k in keep if k in o}}, sys.stdout)
EOF
    chmod 0600 "$FIX/home/.claude/.credentials.json"
fi
[ -z "${CLAUDE_BIN:-}" ] || cp "$CLAUDE_BIN" "$FIX/work/claude"
printf '#!/bin/sh\necho memory\n' > "$FIX/opt/memory/memory"
chmod 0555 "$FIX/opt/memory/memory"
echo "# projection" > "$FIX/projection/README.md"
echo secret > "$FIX/host-home/marker"
CHECK_HOST_MARKER="$FIX/host-home/marker"
export CHECK_HOST_MARKER

python3 -I - "$FIX/run/egress.sock" "${UPSTREAM:-}" <<'EOF' &
import http.client, socketserver, sys, urllib.parse
upstream = urllib.parse.urlsplit(sys.argv[2]) if sys.argv[2] else None
hop = {"host", "connection", "keep-alive", "proxy-connection", "content-length", "transfer-encoding"}
class H(socketserver.StreamRequestHandler):
    def handle(self):
        # read the whole request, so a client still sending its body is not reset
        start = self.rfile.readline().decode()
        headers, length, chunked = [], 0, False
        for line in iter(self.rfile.readline, b""):
            if line in (b"\r\n", b"\n"):
                break
            name, _, value = line.decode().partition(":")
            name, value = name.strip(), value.strip()
            if name.lower() == "content-length":
                length = int(value)
            if name.lower() == "transfer-encoding" and value.lower() == "chunked":
                chunked = True
            headers.append((name, value))
        if len(start.split(" ")) < 3:
            return
        body = b""
        if chunked:
            while size := int(self.rfile.readline().split(b";")[0], 16):
                body += self.rfile.read(size)
                self.rfile.readline()
            self.rfile.readline()
        else:
            body = self.rfile.read(length)
        if upstream is None:
            body = b'{"type":"error","error":{"type":"authentication_error","message":"stand-in egress"}}'
            self.wfile.write(b"HTTP/1.0 401 Unauthorized\r\nContent-Type: application/json\r\n"
                             b"Content-Length: %d\r\n\r\n%s" % (len(body), body))
            return
        method, path = start.split(" ")[:2]
        c = http.client.HTTPSConnection(upstream.hostname, upstream.port or 443, timeout=300)
        c.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        c.putheader("Host", upstream.hostname)
        for name, value in headers:
            if name.lower() not in hop:
                c.putheader(name, value)
        c.putheader("Content-Length", str(len(body)))
        c.endheaders(body)
        r = c.getresponse()
        print(f"egress: {method} {path.split('?')[0]} -> {r.status}", file=sys.stderr, flush=True)
        head = [f"HTTP/1.1 {r.status} {r.reason}"]
        head += [f"{n}: {v}" for n, v in r.getheaders() if n.lower() not in hop]
        self.wfile.write(("\r\n".join(head + ["Connection: close"]) + "\r\n\r\n").encode())
        # streamed as it arrives, so a server-sent event stream is not held back
        while chunk := r.read1(65536):
            self.wfile.write(chunk)
            self.wfile.flush()
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
# a run stopped by a signal still removes the tree, and with it any login copied in: the
# caller's EXIT trap runs only on an exit
trap 'exit 130' INT
trap 'exit 143' TERM
