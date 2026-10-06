#!/bin/sh
# What fetch-wslcsdk.sh refuses: a package that is not the pinned one, a package that
# cannot be read, an architecture the SDK has no DLL for, and no destination. In each case
# nothing may be written, since a DLL beside memory.exe is code it loads.
#
# Then the real package, downloaded once into target/ and checked by its sum like any
# other: both architectures are accepted, and with the two DLL pins swapped each is
# refused, which is the DLL's own sum at work. Without the package or the network that
# half cannot run, and says so rather than passing quietly.
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
grep -q "has sha256 .*nothing written" "$work/err" || fail "a tampered package was not refused by its sum: $(cat "$work/err")"
[ ! -e "$work/out/wslcsdk.dll" ] || fail "a tampered package wrote a DLL"

# a package that cannot be read is refused, not hashed as empty
status=0
WSLCSDK_NUPKG="$work/no-such.nupkg" sh "$script" "$work/out" >/dev/null 2>"$work/err" || status=$?
[ "$status" -eq 1 ] || fail "an unreadable package exited $status, not 1"
grep -q "could not hash" "$work/err" || fail "an unreadable package was not reported as such: $(cat "$work/err")"

# an architecture the SDK has no DLL for
status=0
WSLCSDK_NUPKG="$work/tampered.nupkg" sh "$script" "$work/out" x86 >/dev/null 2>&1 || status=$?
[ "$status" -eq 2 ] || fail "an unknown architecture exited $status, not 2"

# no destination
status=0
sh "$script" >/dev/null 2>&1 || status=$?
[ "$status" -eq 2 ] || fail "no destination exited $status, not 2"
[ ! -e "$work/out" ] || fail "a refused run made the destination"

# the real package: accepted for both architectures, and refused under swapped pins
version=$(sed -n 's/^version=//p' "$script")
cache="$root/target/wslcsdk"
pkg="$cache/microsoft.wsl.containers.$version.nupkg"
if [ ! -f "$pkg" ]; then
    mkdir -p "$cache"
    if curl -fsSL -o "$pkg.part" \
        "https://api.nuget.org/v3-flatcontainer/microsoft.wsl.containers/$version/microsoft.wsl.containers.$version.nupkg" 2>/dev/null; then
        mv "$pkg.part" "$pkg"
    else
        rm -f "$pkg.part"
    fi
fi
if [ -f "$pkg" ]; then
    for arch in x64 arm64; do
        status=0
        WSLCSDK_NUPKG="$pkg" sh "$script" "$work/real-$arch" "$arch" >/dev/null 2>"$work/err" || status=$?
        [ "$status" -eq 0 ] && [ -f "$work/real-$arch/wslcsdk.dll" ] ||
            fail "the real $version package was refused for $arch (a damaged $pkg can be deleted): $(cat "$work/err")"
    done
    x64=$(sed -n 's/^dll_sha256_x64=//p' "$script")
    arm64=$(sed -n 's/^dll_sha256_arm64=//p' "$script")
    sed -e "s/^dll_sha256_x64=.*/dll_sha256_x64=$arm64/" -e "s/^dll_sha256_arm64=.*/dll_sha256_arm64=$x64/" \
        "$script" >"$work/swapped.sh"
    for arch in x64 arm64; do
        status=0
        WSLCSDK_NUPKG="$pkg" sh "$work/swapped.sh" "$work/swapped-$arch" "$arch" >/dev/null 2>&1 || status=$?
        [ "$status" -eq 1 ] || fail "with the DLL pins swapped, $arch exited $status, not 1"
        [ ! -e "$work/swapped-$arch/wslcsdk.dll" ] || fail "with the DLL pins swapped, $arch wrote a DLL"
    done
else
    echo "fetch-wslcsdk.test.sh: no package in $cache and no network: the real package, and the DLL sums, were NOT checked" >&2
fi

if [ "$failures" -ne 0 ]; then
    exit 1
fi
echo "fetch-wslcsdk.sh: every case holds"
