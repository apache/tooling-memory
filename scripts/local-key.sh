#!/bin/sh
# Mint one virtual key on the local LiteLLM with the master key, and keep it in
# local/.virtual-key (ignored by git). This is what llmao's front desk does for a
# committer; locally the master key does it once.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
# shellcheck disable=SC1090
set -a; . "$here/local/.env"; set +a
key_file="$here/local/.virtual-key"

if [ -s "$key_file" ]; then
    echo "virtual key already at $key_file"
    exit 0
fi

response=$(curl -sS -X POST http://127.0.0.1:4000/key/generate \
    -H "Authorization: Bearer $LITELLM_MASTER_KEY" \
    -H "Content-Type: application/json" \
    -d '{"key_alias": "local-claude-code", "metadata": {"purpose": "claude code on this laptop"}}')
key=$(printf '%s' "$response" | python3 -c 'import sys,json; print(json.load(sys.stdin)["key"])') || {
    echo "key/generate did not return a key:" >&2
    printf '%s\n' "$response" >&2
    exit 1
}
umask 077
printf '%s\n' "$key" > "$key_file"
echo "virtual key written to $key_file"
