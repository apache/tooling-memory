# Running it locally

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
