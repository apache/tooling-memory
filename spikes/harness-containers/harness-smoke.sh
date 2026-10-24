#!/bin/sh
# Inside a box: does a Node harness start, and do its model calls reach the egress through
# a loopback relay, as `amem relay` would forward them? The relay here is a few lines of
# Node that count connections made through to the egress socket; the stand-in egress also
# logs each request it receives, outside the box. It answers 401, which Claude Code
# retries, so the run ends on the timeout; what matters is that the calls arrived.
node -e '
const net = require("net");
let n = 0;
net.createServer(c => {
  const u = net.connect("/run/memory/egress.sock", () =>
    require("fs").writeFileSync(process.env.HOME + "/relay.count", String(++n)));
  c.pipe(u); u.pipe(c);
  u.on("error", () => c.destroy());
  c.on("error", () => u.destroy());
}).listen(8080, "127.0.0.1");
' &
sleep 1
for h in claude codex gemini; do printf '%s: %s\n' "$h" "$("$h" --version 2>&1 | head -1)"; done
ANTHROPIC_BASE_URL=http://127.0.0.1:8080 ANTHROPIC_API_KEY=sk-placeholder \
    timeout 20 claude -p hello < /dev/null > /dev/null 2>&1
echo "claude: exit $?, $(cat "$HOME/relay.count" 2>/dev/null || echo 0) calls reached the egress through the relay"
