#!/bin/sh
# Inside a box: one real model call. A loopback relay forwards to the egress, as `amem
# relay` would: the unix socket, or vsock where CHECK_SOCKET says `vsock:CID:PORT`. Claude
# Code, on the login the fixture copied in, is asked for one word; a Claude Code binary the
# fixture copied into /work is used where the image has none. Run with the fixture's
# UPSTREAM and LOGIN set, or the stand-in answers 401.
target=${CHECK_SOCKET:-/run/memory/egress.sock}
if command -v python3 >/dev/null; then
    python3 -I -c '
import socket, sys, threading
target = sys.argv[1]
def connect():
    if target.startswith("vsock:"):
        _, cid, port = target.split(":")
        s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        s.connect((int(cid), int(port)))
    else:
        s = socket.socket(socket.AF_UNIX)
        s.connect(target)
    return s
def pipe(a, b):
    try:
        while data := a.recv(65536):
            b.sendall(data)
    except OSError:
        pass
    try:
        b.shutdown(socket.SHUT_WR)
    except OSError:
        pass
server = socket.create_server(("127.0.0.1", 8080))
while True:
    c, _ = server.accept()
    u = connect()
    threading.Thread(target=pipe, args=(c, u), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c), daemon=True).start()
' "$target" &
else
    node -e '
const net = require("net");
net.createServer(c => {
  const u = net.connect(process.argv[1]);
  c.pipe(u); u.pipe(c);
  u.on("error", () => c.destroy());
  c.on("error", () => u.destroy());
}).listen(8080, "127.0.0.1");
' "$target" &
fi
relay=$!
sleep 1
PATH=$PATH:/work
unset ANTHROPIC_API_KEY
printf 'claude %s\n' "$(claude --version 2>&1 | head -1)"
ANTHROPIC_BASE_URL=http://127.0.0.1:8080 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    timeout 120 claude -p 'Reply with exactly one word: pong' < /dev/null 2>&1
echo "claude: exit $?"
# a box whose exec waits for every holder of its output (microsandbox) waits on the relay
kill "$relay"
