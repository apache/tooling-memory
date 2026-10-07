#!/bin/sh
# The server under test: a fresh record with a few entries, and `amem web` over it with
# every session running session.sh in place of the launcher, so the pages are driven
# without a harness or a sandbox. Playwright starts it (playwright.config.ts) and the
# tests read the token from .state/home/memory/state/web.json. A hosted copy of the
# pages, served by support/static.mjs, is let through from its origin.
set -eu
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
amem=${AMEM:-$here/../target/debug/amem}
state=$here/.state
root=$state/home/memory
rm -rf "$state"
mkdir -p "$state/home" "$state/work"
export HOME="$state/home"
unset MEMORY_ROOT BROWSER

"$amem" --root "$root" init --owner human:e2e:tester --domain dom-e2e >/dev/null
write() {
    "$amem" --root "$root" write --kind "$1" --title "$2" --body "$3" --classification "$4" |
        sed -n 's/.*"id": *"\(mem-[^"]*\)".*/\1/p'
}
write decision "Leases expire at a fixed horizon" \
    "A claim is one transaction over the ledger. The lease it grants runs to a horizon set at the claim and is never extended past it." \
    internal >"$state/ids"
write fact "The ledger's clock is its own" \
    "Lease horizons are read from the ledger's clock, not the holder's." internal >>"$state/ids"
write convention "Commit subjects carry a prefix" \
    "feat, fix, docs, test, refactor, chore or ci, then a colon." public >>"$state/ids"

cd "$state/work"
exec "$amem" --root "$root" web --listen "127.0.0.1:${PORT:-4310}" --no-open --backend none \
    --hosted "http://localhost:${HOSTED_PORT:-4311}/" \
    --session-command "sh $here/session.sh $state/ids $root"
