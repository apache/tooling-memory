#!/bin/sh
# The pages as plain files, for a host that serves files and nothing else: the same
# pages the binary embeds, which find their server in the pairing link amem opens
# (`amem --hosted <url>`), so the host never sees the server or the key.
#   scripts/hosted.sh [out]        default target/hosted
set -eu
here=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
out=${1:-$here/target/hosted}
static=$here/crates/web/static
# only a directory this script made is replaced: it leaves a marker of its own in each
# one (.nojekyll would not do: every hand-made Pages site has one)
if [ -e "$out" ] && [ ! -e "$out/.amem-hosted" ] && [ -n "$(ls -A "$out")" ]; then
    echo "$out is there and was not made by this script; give an empty or new directory" >&2
    exit 2
fi
# the reader's glue and wasm are not in the tree: dm-web's build script builds them into
# its OUT_DIR, which cargo names in its messages. The debug build of amem, as e2e runs it.
# The messages go to a file, not a pipe, so a failed build stops here with its status;
# rustc's diagnostics go to stderr as text.
messages=$(mktemp)
trap 'rm -f "$messages"' EXIT
cargo build --manifest-path "$here/Cargo.toml" -p amem \
    --message-format=json-render-diagnostics >"$messages"
built=$(python3 -c '
import json, sys
for line in open(sys.argv[1]):
    m = json.loads(line)
    if m.get("reason") == "build-script-executed" and "#dm-web@" in m["package_id"]:
        print(m["out_dir"])
' "$messages")
if [ -z "$built" ] || [ ! -s "$built/reader/dm_reader_bg.wasm" ]; then
    echo "no reader from the build of amem; see cargo's errors above" >&2
    exit 1
fi
rm -rf "$out"
mkdir -p "$out/vendor" "$out/reader"
for f in index.html session.html board.html reader.html amem.js tokens.css; do
    cp "$static/$f" "$out/$f"
done
cp "$static"/vendor/* "$out/vendor/"
cp "$built/reader/dm_reader.js" "$built/reader/dm_reader_bg.wasm" "$out/reader/"
# a host that runs Jekyll would drop the files it does not know
: > "$out/.nojekyll"
: > "$out/.amem-hosted"
echo "hosted pages in $out"
