"""One tool call through `amem serve` over stdio, as the author given: how the demo
speaks as an agent without a harness. A held task, if given, is in MEMORY_TASK, as the
launcher sets it for a harness working a task.

    python3 mcp.py <amem> <root> <author> <tool> '<json arguments>' [task-id]
    python3 mcp.py <amem> <root> <author> batch '[{"tool": t, "arguments": {...}}, ...]'

Prints the tool's JSON result, or `refused: <message>` and exits 1. A batch is several
calls in one session, as a harness makes them; it prints the last result."""
import json
import os
import subprocess
import sys

amem, root, author, tool, args = sys.argv[1:6]
env = dict(os.environ)
if len(sys.argv) > 6 and sys.argv[6]:
    env["MEMORY_TASK"] = sys.argv[6]
p = subprocess.Popen([amem, "--root", root, "serve", "--author", author],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)


def send(m):
    p.stdin.write(json.dumps(m) + "\n")
    p.stdin.flush()


def recv():
    return json.loads(p.stdout.readline())


send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
      "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": {"name": "demo", "version": "0"}}})
recv()
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
calls = json.loads(args) if tool == "batch" else [{"tool": tool, "arguments": json.loads(args)}]
r = None
for n, c in enumerate(calls, start=2):
    send({"jsonrpc": "2.0", "id": n, "method": "tools/call",
          "params": {"name": c["tool"], "arguments": c["arguments"]}})
    r = recv()
    if "error" in r or r["result"].get("isError"):
        break
p.stdin.close()
p.wait()
if "error" in r:
    print("refused: " + r["error"]["message"])
    sys.exit(1)
res = r["result"]
if res.get("isError"):
    print("refused: " + " ".join(c.get("text", "") for c in res.get("content", [])))
    sys.exit(1)
text = "".join(c.get("text", "") for c in res.get("content", []))
print(text)
