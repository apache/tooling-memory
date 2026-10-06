#!/bin/sh
# What fetch-wslcsdk.sh refuses, without the network: a package that is not the pinned
# one, an architecture the SDK has no DLL for, and no destination. In each case nothing
# may be written, since a DLL beside memory.exe is code it loads. The last check holds the
# premise that the version local/README.md names is the one pinned here.
#
# Run by `make check`.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "$here/.." && pwd)
script="$here/fetch-wslcsdk.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

fail() {
    printf '%s\n' "$1" >&2
    failures=$((failures + 1))
}

# a package that is not the pinned one, whatever it holds
printf 'not the package\n' >"$work/tampered.nupkg"
status=0
WSLCSDK_NUPKG="$work/tampered.nupkg" sh "$script" "$work/out" >/dev/null 2>"$work/err" || status=$?
[ "$status" -eq 1 ] || fail "a tampered package exited $status, not 1"
grep -q "nothing written" "$work/err" || fail "a tampered package was not refused by its sum: $(cat "$work/err")"
[ ! -e "$work/out/wslcsdk.dll" ] || fail "a tampered package wrote a DLL"

# an architecture the SDK has no DLL for
status=0
WSLCSDK_NUPKG="$work/tampered.nupkg" sh "$script" "$work/out" x86 >/dev/null 2>&1 || status=$?
[ "$status" -eq 2 ] || fail "an unknown architecture exited $status, not 2"

# no destination
status=0
sh "$script" >/dev/null 2>&1 || status=$?
[ "$status" -eq 2 ] || fail "no destination exited $status, not 2"
[ ! -e "$work/out" ] || fail "a refused run made the destination"

# the version the setup notes name is the one pinned
pinned=$(sed -n 's/^version=//p' "$script")
grep -q "Microsoft.WSL.Containers\` NuGet package, version $pinned" "$root/local/README.md" ||
    fail "local/README.md does not name version $pinned of the package"

if [ "$failures" -ne 0 ]; then
    exit 1
fi
echo "fetch-wslcsdk.sh: every case holds"
