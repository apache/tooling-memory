#!/bin/sh
# Bump the harness pins to the latest published versions: the npm harnesses in
# images/harness/package.json, with package-lock.json regenerated beside it, and pi and Node
# in images/harness/Dockerfile. Prints one line per change, and a line for each override in
# package.json that a bump made stale or that now holds a harness back; exits 0 whether or
# not anything changed. Needs npm and curl; without Node on the host, run it in node:24
# (images/harness/README.md). A registry or nodejs.org that does not answer is an error,
# never "nothing to bump".
set -eu
dir="$(cd "$(dirname "$0")/../images/harness" && pwd)"
df="$dir/Dockerfile"
command -v npm >/dev/null 2>&1 || {
  echo "bump-harness-versions: needs npm; run it in node:24, as images/harness/README.md says" >&2
  exit 1
}
latest() { # npm package
  v=$(npm view "$1@latest" version) || { echo "bump-harness-versions: npm view $1 failed" >&2; exit 1; }
  [ -n "$v" ] || { echo "bump-harness-versions: the registry gave no version for $1" >&2; exit 1; }
  echo "$v"
}
bump() { # ARG name, npm package
  cur=$(sed -n "s/^ARG $1=\(.*\)$/\1/p" "$df")
  new=$(latest "$2") || exit 1
  if [ "$new" != "$cur" ]; then
    sed -i "s/^ARG $1=.*$/ARG $1=$new/" "$df"
    echo "$1: $cur -> $new ($2)"
  fi
}
moved=0
bump_npm() { # npm package, as package.json's dependencies name it
  cur=$(node -p 'require(process.argv[1]).dependencies[process.argv[2]]' "$dir/package.json" "$1")
  new=$(latest "$1") || exit 1
  if [ "$new" != "$cur" ]; then
    # package.json and the lock are written together, or neither is
    (cd "$dir" && npm install --package-lock-only --save-exact --no-fund --no-audit "$1@$new" >/dev/null)
    echo "$1: $cur -> $new"
    moved=1
  fi
}
bump_npm @anthropic-ai/claude-code
bump_npm @openai/codex
bump_npm @google/gemini-cli
bump_npm @github/copilot
bump_npm opencode-ai
bump_npm @qwen-code/qwen-code
bump_npm @xai-official/grok
if [ "$moved" = 1 ]; then
  # An override holds a package under one harness at a version the harness does not ask for,
  # and npm says nothing when it applies. Say when one is no longer needed, or holds back a
  # version the harness now asks for.
  node -e '
    const [pj, lock] = process.argv.slice(1).map(f => require(f));
    const parts = v => v.split(".").map(Number);
    const older = (a, b) => { const x = parts(a), y = parts(b);
      for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] < y[i]; return false; };
    for (const [parent, held] of Object.entries(pj.overrides || {})) {
      if (typeof held !== "object") continue;
      const p = lock.packages["node_modules/" + parent] || {};
      const asks = { ...p.dependencies, ...p.optionalDependencies, ...p.peerDependencies };
      for (const [dep, v] of Object.entries(held)) {
        const a = asks[dep];
        if (a === undefined) console.log(`override: ${parent} no longer depends on ${dep}; remove the override`);
        else if (!/^\d+\.\d+\.\d+$/.test(a)) console.log(`override: ${parent} now asks for ${dep} ${a}, and the override holds ${v}; check it`);
        else if (!older(a, v)) console.log(`override: ${parent} now asks for ${dep} ${a}, and the override holds ${v}; remove the override`);
      }
    }' "$dir/package.json" "$dir/package-lock.json"
fi
bump PI @earendil-works/pi-coding-agent
# Node: the latest release on the LTS line the Dockerfile is on
cur=$(sed -n 's/^ARG NODE_VERSION=\(.*\)$/\1/p' "$df")
major=${cur%%.*}
index=$(curl -fsSL https://nodejs.org/dist/index.json) || {
  echo "bump-harness-versions: nodejs.org's release index did not answer" >&2
  exit 1
}
new=$(printf '%s' "$index" | tr -d '\n' | sed 's/},{/}\n{/g' | grep "\"version\":\"v$major\." | head -1 | sed 's/.*"version":"v\([^"]*\)".*/\1/' || true)
[ -n "$new" ] || { echo "bump-harness-versions: no Node $major release in nodejs.org's index" >&2; exit 1; }
if [ "$new" != "$cur" ]; then
  sed -i "s/^ARG NODE_VERSION=.*$/ARG NODE_VERSION=$new/" "$df"
  echo "NODE_VERSION: $cur -> $new"
fi
