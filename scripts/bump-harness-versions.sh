#!/bin/sh
# Bump the harness pins in images/harness/Dockerfile to the latest published versions.
# Prints one line per change; exits 0 whether or not anything changed. Needs npm and curl.
set -eu
df="$(dirname "$0")/../images/harness/Dockerfile"
bump() { # ARG name, npm package
  cur=$(sed -n "s/^ARG $1=\(.*\)$/\1/p" "$df")
  new=$(npm view "$2" version 2>/dev/null | tail -1)
  if [ -n "$new" ] && [ "$new" != "$cur" ]; then
    sed -i "s/^ARG $1=.*$/ARG $1=$new/" "$df"
    echo "$1: $cur -> $new ($2)"
  fi
}
bump CLAUDE_CODE @anthropic-ai/claude-code
bump CODEX @openai/codex
bump GEMINI_CLI @google/gemini-cli
bump COPILOT @github/copilot
bump OPENCODE opencode-ai
# Node: the latest release on the LTS line the Dockerfile is on
cur=$(sed -n 's/^ARG NODE_VERSION=\(.*\)$/\1/p' "$df")
major=${cur%%.*}
new=$(curl -fsSL https://nodejs.org/dist/index.json | tr -d '\n' | sed 's/},{/}\n{/g' | grep "\"version\":\"v$major\." | head -1 | sed 's/.*"version":"v\([^"]*\)".*/\1/')
if [ -n "$new" ] && [ "$new" != "$cur" ]; then
  sed -i "s/^ARG NODE_VERSION=.*$/ARG NODE_VERSION=$new/" "$df"
  echo "NODE_VERSION: $cur -> $new"
fi
