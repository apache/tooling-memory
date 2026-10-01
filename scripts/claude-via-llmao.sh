#!/bin/sh
# Start Claude Code through the local gateway, with the memory server attached.
#
# Reads local/.env for the root and the key. Every model call goes to LiteLLM on
# localhost:4000 under a virtual key, so your provider key never reaches the harness; the
# memory server runs as a sidecar the harness reaches over stdio and holds the only
# access to the memory root. Extra arguments are passed to claude.
#
#   scripts/claude-via-llmao.sh                 interactive
#   scripts/claude-via-llmao.sh -p "load T0"    one prompt

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
env_file="$here/local/.env"
key_file="$here/local/.virtual-key"

if [ ! -f "$env_file" ]; then
    echo "no $env_file; copy local/.env.example and fill it in" >&2
    exit 1
fi
# shellcheck disable=SC1090
set -a; . "$env_file"; set +a

if [ ! -f "$key_file" ]; then
    echo "no virtual key yet; run: make local-key" >&2
    exit 1
fi
: "${MEMORY_ROOT:?set MEMORY_ROOT in local/.env}"

bin="$here/target/release/memory"
if [ ! -x "$bin" ]; then
    echo "no $bin; run: cargo build --release" >&2
    exit 1
fi
if [ ! -f "$MEMORY_ROOT/grants.toml" ]; then
    echo "no memory root at $MEMORY_ROOT; run: make local-memory" >&2
    exit 1
fi

mcp_config=$(mktemp)
trap 'rm -f "$mcp_config"' EXIT
cat > "$mcp_config" <<JSON
{
  "mcpServers": {
    "memory": {
      "command": "$bin",
      "args": ["--root", "$MEMORY_ROOT", "serve", "--author", "agent:claude-code"]
    }
  }
}
JSON

ANTHROPIC_BASE_URL="http://127.0.0.1:4000" \
ANTHROPIC_AUTH_TOKEN="$(cat "$key_file")" \
exec claude --mcp-config "$mcp_config" "$@"
