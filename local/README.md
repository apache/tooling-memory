# Running it locally

## Confined: `memory launch`

The way to run a harness. It needs no containers. The harness gets its own user, mount,
network, pid, uts and ipc namespaces, built natively by the `memory` binary with no
privilege: the system's binaries read-only, a private home, the working tree read-write,
the memory binary, and the egress socket. No network interface except loopback. Inside,
a relay forwards one loopback port to the egress, which serves memory over MCP and
forwards model calls to the provider or the gateway, checks the session label against the
destination's ceiling first, and reads token usage out of every response.

```sh
make launch                                   # claude, your own login, confined
make launch HARNESS=codex AUTH=key            # codex, OPENAI_API_KEY from local/.env
make launch AUTH=gateway                      # claude through the local LiteLLM
make launch ARGS='-p "load T0 and summarise"' # arguments go to the harness
make usage                                    # tokens by destination and model
```

| `AUTH` | Model calls go to | Credential | Who holds it |
|---|---|---|---|
| `subscription` | api.anthropic.com | your Claude Code login | the harness, copied into its private home |
| `key` | the provider | `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` from `local/.env` | the egress; the harness gets a placeholder |
| `gateway` | LiteLLM on 127.0.0.1:4000 | `local/.virtual-key` | the egress; the harness gets a placeholder |

In every mode the session's handling is capped at the destination's ceiling from
`grants.toml`, so a session whose calls go to a destination with a public ceiling reads only
public entries. Codex with a ChatGPT login is not supported yet: Codex routes that to
chatgpt.com, which the egress does not proxy.

`--unconfined` runs the same configuration without namespaces, with the egress on a TCP
port, for platforms without a backend. macOS gets Seatbelt later; on Windows run under WSL2.

The rest of this page is the gateway half of the stack.

The local stack is the Foundation's shape at laptop size: LiteLLM as the gateway with
Postgres behind it, the memory server as a sidecar beside the harness, and Claude Code as
the harness. Your provider key sits in LiteLLM and nowhere else. The harness gets a
virtual key for model calls and a stdio connection to the memory server, and that is all
it gets.

```
  claude ──ANTHROPIC_BASE_URL──▶ litellm:4000 ──ANTHROPIC_API_KEY──▶ Anthropic
    │                               └── postgres (keys, spend)
    └──stdio──▶ memory serve --root $MEMORY_ROOT
```

## Once

Podman (or Docker) with compose, Rust stable, and Claude Code on your PATH.

```sh
cp local/.env.example local/.env      # fill in ANTHROPIC_API_KEY and MEMORY_ROOT
cargo build --release
make local-up                         # LiteLLM and Postgres, on 127.0.0.1:4000
make local-key                        # mints a virtual key into local/.virtual-key
make local-memory                     # memory init at MEMORY_ROOT, if not there yet
```

## Every time

```sh
scripts/claude-via-llmao.sh
```

Claude Code starts with every model call going through the gateway and the `memory`
server attached. Ask it to load T0, or let it discover the `t0` tool from the server's
instructions. When you stop, ask it to write a handoff; the next session finds it.

`make local-down` stops the containers and keeps the Postgres volume. `make local-logs`
tails LiteLLM.

## What this is not

Not the asfquart front desk. At the Foundation, llmao issues a PAT against ASF identity
and maps projects to LiteLLM teams; here the master key mints one key and you are the only
principal. The gateway, the sidecar, and the harness are the same pieces in the same
places, which is the point.

## Checks

```sh
curl -s http://127.0.0.1:4000/health/liveliness
curl -s http://127.0.0.1:4000/v1/models -H "Authorization: Bearer $(cat local/.virtual-key)"
```

The second lists what the key may call; with the wildcard route in `local/litellm.yaml` it
is empty, which is fine. A model id LiteLLM cannot resolve returns a 400 with the id in the
message; add an explicit entry for it. To prove the route without spending anything, send
one request with a wrong provider key in `local/.env`: the error that comes back is
Anthropic's own "API key is invalid", relayed by the gateway.

On Podman with the CNI network backend, compose DNS between containers is off, so LiteLLM
shares Postgres's network namespace instead of resolving a service name. That is why the
gateway's port is published on the `postgres` service.
