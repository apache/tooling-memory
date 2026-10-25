#!/bin/sh
# Bump the harness pins to the latest published versions: the npm harnesses in
# images/harness/package.json, with package-lock.json regenerated to match, and pi and Node
# in images/harness/Dockerfile. Prints one line per change; exits 0 whether or not anything
# changed. Needs curl, and npm to regenerate the lock; without npm, the registry is asked
# directly, and a bump to package.json fails rather than leave the lock behind it.
set -eu
dir="$(dirname "$0")/../images/harness"
df="$dir/Dockerfile"
pj="$dir/package.json"
latest() { # npm package
  if command -v npm >/dev/null 2>&1; then
    npm view "$1" version 2>/dev/null | tail -1
  else
    curl -fsSL "https://registry.npmjs.org/$1/latest" 2>/dev/null |
      sed -n 's/.*"version":"\([^"]*\)".*/\1/p' | head -1
  fi
}
bump() { # ARG name, npm package
  cur=$(sed -n "s/^ARG $1=\(.*\)$/\1/p" "$df")
  new=$(latest "$2")
  if [ -n "$new" ] && [ "$new" != "$cur" ]; then
    sed -i "s/^ARG $1=.*$/ARG $1=$new/" "$df"
    echo "$1: $cur -> $new ($2)"
  fi
}
locked=0
bump_npm() { # npm package, as package.json's dependencies name it
  cur=$(sed -n "s|^    \"$1\": \"\(.*\)\",\{0,1\}$|\1|p" "$pj")
  new=$(latest "$1")
  if [ -n "$new" ] && [ "$new" != "$cur" ]; then
    sed -i "s|^    \"$1\": \"$cur\"|    \"$1\": \"$new\"|" "$pj"
    echo "$1: $cur -> $new"
    locked=1
  fi
}
bump_npm @anthropic-ai/claude-code
bump_npm @openai/codex
bump_npm @google/gemini-cli
bump_npm @github/copilot
bump_npm opencode-ai
bump_npm @qwen-code/qwen-code
bump_npm @xai-official/grok
if [ "$locked" = 1 ]; then
  command -v npm >/dev/null 2>&1 || { echo "npm is needed to regenerate package-lock.json" >&2; exit 1; }
  (cd "$dir" && npm install --package-lock-only --no-fund --no-audit >/dev/null)
fi
bump PI @earendil-works/pi-coding-agent
# Node: the latest release on the LTS line the Dockerfile is on
cur=$(sed -n 's/^ARG NODE_VERSION=\(.*\)$/\1/p' "$df")
major=${cur%%.*}
new=$(curl -fsSL https://nodejs.org/dist/index.json | tr -d '\n' | sed 's/},{/}\n{/g' | grep "\"version\":\"v$major\." | head -1 | sed 's/.*"version":"v\([^"]*\)".*/\1/')
if [ -n "$new" ] && [ "$new" != "$cur" ]; then
  sed -i "s/^ARG NODE_VERSION=.*$/ARG NODE_VERSION=$new/" "$df"
  echo "NODE_VERSION: $cur -> $new"
fi
