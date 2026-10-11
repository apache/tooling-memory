#!/bin/sh
# Get the demo to a state it can run, and say what it found:
#
#   scripts/demo-ready.sh <demo-root> <markdown-dir>...      (make demo-ready)
#
# The record: made and seeded when there is none, as the first act would; RESET=1
# starts it over. The harness image: unpacked into the record, so the sessions run the
# harness versions images/harness/Dockerfile pins, built first when there is none and
# rebuilt when the unpacked one is not the pinned Claude Code (IMAGE_REBUILD=1 forces
# it). The gateway: its key, and whether it answers. The logins: what `amem doctor`
# finds. Podman: whether it can run in this shell, which the demo does not need here
# and the local stack does. Exits 0 when the demo can run, 1 when something it needs is
# missing; each line says what and how to fix it.
set -eu
root=${1:?demo root}; shift
here=$(cd "$(dirname "$0")/.." && pwd)
memory="$here/target/release/amem"
image=${IMAGE:-durable-memory/harness:dev}
missing=0
ok() { printf '  ok    %s\n' "$*"; }
warn() { printf '  --    %s\n' "$*"; }
need() { printf '  NO    %s\n' "$*"; missing=1; }

[ -x "$memory" ] || { echo "build first: make build" >&2; exit 2; }
echo "demo-ready · $root"

# ---- the record
if [ "${RESET:-}" = 1 ] && [ -f "$root/grants.toml" ]; then
  # only a demo's record goes: its owner is human:demo:<you>
  if grep -q '^owner = "human:demo:' "$root/grants.toml"; then
    rm -rf "$root"
    echo "  reset: $root removed"
  else
    need "RESET=1 refused: $root is not a demo record (its owner is not human:demo:...)"
    exit 1
  fi
fi
if [ ! -f "$root/grants.toml" ]; then
  [ $# -ge 1 ] || { need "no record at $root and no Markdown to seed it from"; exit 1; }
  "$memory" --root "$root" init --owner "human:demo:$(id -un)" --domain dom-demo >/dev/null
  python3 "$here/scripts/seed-corpus.py" --root "$root" --memory "$memory" "$@" | tail -n 1
fi
n=$(find "$root/record" -mindepth 2 -maxdepth 2 -name '*.md' | wc -l | tr -d ' ')
ok "record           $n entries"

# ---- the harness image, unpacked into the record
pinned=$(sed -n 's/^ARG CLAUDE_CODE=\(.*\)$/\1/p' "$here/images/harness/Dockerfile")
installed() {
  r=$("$memory" --root "$root" image path 2>/dev/null) || return 0
  f=$(find "$r/usr" -path '*node_modules/@anthropic-ai/claude-code/package.json' 2>/dev/null | head -n 1)
  [ -n "$f" ] && sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$f" | head -n 1
  return 0
}
have=$(installed)
if [ "${IMAGE_REBUILD:-}" = 1 ] || [ "$have" != "$pinned" ]; then
  # the image as built, if it is the pinned one; else build it
  if [ "${IMAGE_REBUILD:-}" != 1 ] && "$memory" --root "$root" image unpack --image "$image" >/dev/null 2>&1; then
    have=$(installed)
  fi
  if [ "$have" != "$pinned" ]; then
    echo "  building the harness image ($image, Claude Code $pinned); a few minutes"
    if make -s -C "$here" image IMAGE="$image" >"$root/state/image-build.log" 2>&1 \
      && "$memory" --root "$root" image unpack --image "$image" >/dev/null 2>&1; then
      have=$(installed)
    else
      warn "image            could not be built (see $root/state/image-build.log); sessions run this host's harnesses, confined by namespaces"
    fi
  fi
fi
[ "$have" = "$pinned" ] && ok "image            unpacked, Claude Code $have as pinned"

# ---- the gateway, for acts 2, 3, 5 and 6
url=${LLMAO_URL:-https://llm.apache.org}
key=${LLMAO_KEY:-$(cat "$here/local/.virtual-key" 2>/dev/null || true)}
if [ -z "$key" ]; then
  warn "gateway          no key in local/.virtual-key: the gateway's acts will say skipped"
elif code=$(curl -s -o /dev/null -w '%{http_code}' -m 15 -H "Authorization: Bearer $key" "$url/v1/models") \
  && [ "$code" = 200 ]; then
  ok "gateway          $url answers with the key"
else
  need "gateway          $url answered ${code:-nothing} with the key in local/.virtual-key"
fi

# ---- the logins, as doctor finds them
"$memory" --root "$root" doctor 2>/dev/null | sed -n '/^logins/,/^$/p' | grep -E '^  (ok|--) ' || true
if [ -n "${HARNESS:-}" ]; then
  bin=$HARNESS; [ "$bin" = antigravity ] && bin=agy
  if "$memory" --root "$root" doctor 2>/dev/null | grep -qE "^  ok +$bin "; then
    ok "$(printf "%-16s" "$HARNESS") logged in, as --harness $HARNESS needs"
  else
    need "$HARNESS has no login doctor can see: log in on the host first"
  fi
fi

# ---- podman, which the local stack needs and this demo does not
run=${XDG_RUNTIME_DIR:-}
if ! command -v podman >/dev/null 2>&1; then
  warn "podman           not installed: make local-up will not run"
elif [ -n "$run" ] && [ ! -d "$run" ]; then
  warn "podman           cannot run in this shell: $run is missing, since no login session made it."
  warn "                 Once, then a new terminal: sudo loginctl enable-linger $(id -un)"
else
  ok "podman           runtime dir $run"
fi

echo
if [ "$missing" = 0 ]; then
  echo "ready: make demo-all HARNESS=${HARNESS:-claude} DEMO_ROOT=$root"
else
  echo "not ready: fix the NO lines above"
fi
exit "$missing"
