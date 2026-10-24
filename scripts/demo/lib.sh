# Shared by the demo's acts: the binary, the record, the gateway, and a few helpers.
# Each act is `scripts/demo/<n>-<name>.sh <demo-root>`, runnable alone on a record the
# first act seeded, or all together through scripts/demo-all.sh.
set -eu
root=${1:?demo root}
here=$(cd "$(dirname "$0")/../.." && pwd)
memory="$here/target/release/amem"
[ -x "$memory" ] || { echo "build first: make build" >&2; exit 2; }
[ -f "$root/grants.toml" ] || { echo "no record at $root: run the first act, make demo" >&2; exit 2; }
a() { "$memory" --root "$root" "$@"; }
# a field of the JSON on stdin, as a Python expression over `d`
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# a tool call as an agent: author, tool, JSON arguments, optional held task
agent() { python3 "$here/scripts/demo/mcp.py" "$memory" "$root" "$@"; }
. "$here/scripts/demo/show.sh"
# the gateway: the Foundation's, with the PAT kept in local/.virtual-key
export LLMAO_URL=${LLMAO_URL:-https://llm.apache.org}
export LLMAO_KEY=${LLMAO_KEY:-$(cat "$here/local/.virtual-key" 2>/dev/null || true)}
gateway() { [ -n "$LLMAO_KEY" ]; }
# the models the acts use on the gateway: a checker, and a cheap worker
CHECKER_MODEL=${CHECKER_MODEL:-gemma4-26b}
WORKER_MODEL=${WORKER_MODEL:-qwen3.8-27b}
WORKER_HARNESS=${WORKER_HARNESS:-codex}
# A gateway run has to name the model: the gateway serves what it serves, and pi and
# OpenCode pick from their own providers' lists rather than taking a default ("pi picks a
# model from its providers' lists, so a run needs --model"). A subscription run must NOT
# have one forced on it -- a vendor's own model names are not the gateway's -- so this
# stays empty unless the run is on the gateway or MODEL was set by hand.
[ "${AUTH:-subscription}" = gateway ] && MODEL=${MODEL:-$WORKER_MODEL}
MODEL_ARG=${MODEL:+--model $MODEL}
