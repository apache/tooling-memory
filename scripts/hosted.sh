#!/bin/sh
# The pages as plain files, for a host that serves files and nothing else: the same
# pages the binary embeds, which find their server in the pairing link amem opens
# (`amem --hosted <url>`), so the host never sees the server or the key.
#   scripts/hosted.sh [out]        default target/hosted
set -eu
here=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
out=${1:-$here/target/hosted}
static=$here/crates/web/static
# only a directory this script made is replaced: it leaves .nojekyll in each one
if [ -e "$out" ] && [ ! -e "$out/.nojekyll" ] && [ -n "$(ls -A "$out")" ]; then
    echo "$out is there and was not made by this script; give an empty or new directory" >&2
    exit 2
fi
rm -rf "$out"
mkdir -p "$out/vendor" "$out/reader"
for f in index.html session.html board.html reader.html amem.js tokens.css; do
    cp "$static/$f" "$out/$f"
done
cp "$static"/vendor/* "$out/vendor/"
cp "$static"/reader/* "$out/reader/"
# a host that runs Jekyll would drop the files it does not know
: > "$out/.nojekyll"
echo "hosted pages in $out"
