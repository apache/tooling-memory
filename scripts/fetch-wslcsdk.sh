#!/bin/sh
# Fetch wslcsdk.dll, the WSL containers C API, which does not ship with WSL: it comes in
# the Microsoft.WSL.Containers NuGet package, and the Windows `amem.exe` loads it from
# beside itself. The package and the DLL are pinned by version and sha256, and nothing is
# written unless both match.
#
#   scripts/fetch-wslcsdk.sh <directory beside amem.exe> [x64|arm64]
#
# WSLCSDK_NUPKG names a package already on disk instead of downloading it. To move to a
# new SDK, change the version and all three sums together, from a package you checked.

set -eu

version=3.0.1
nupkg_sha256=aa051d97078b004cefe5f1ef22c0eec34d917f8802be1784159d3b9c4d916072
dll_sha256_x64=f3528a5b69b777d2bf606edc1629d13ca549f1b87c9f57380dd2f2c611b30b4f
dll_sha256_arm64=7afecfc4fc5d3133172e8c0fba763ff8266473d4f78b4cfb20386eb2865b1ab4

if [ $# -lt 1 ] || [ -z "$1" ]; then
    echo "usage: $0 <directory beside amem.exe> [x64|arm64]" >&2
    exit 2
fi
dest=$1
arch=${2:-x64}
case $arch in
x64) dll_sha256=$dll_sha256_x64 ;;
arm64) dll_sha256=$dll_sha256_arm64 ;;
*)
    echo "fetch-wslcsdk: no such architecture: $arch (x64 or arm64)" >&2
    exit 2
    ;;
esac

# Stop, writing nothing, unless the file's sha256 is the one pinned for it. Its variables
# are prefixed, since sh has no locals and a plain name would overwrite the script's.
require_sum() {
    rs_file=$1 rs_want=$2 rs_what=$3
    if command -v sha256sum >/dev/null 2>&1; then
        rs_got=$(sha256sum "$rs_file" | cut -d' ' -f1)
    else
        rs_got=$(shasum -a 256 "$rs_file" | cut -d' ' -f1)
    fi
    if [ -z "$rs_got" ]; then
        echo "fetch-wslcsdk: could not hash $rs_what ($rs_file); nothing written" >&2
        exit 1
    fi
    if [ "$rs_got" != "$rs_want" ]; then
        echo "fetch-wslcsdk: $rs_what has sha256 $rs_got, not $rs_want as pinned; nothing written" >&2
        exit 1
    fi
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

pkg=${WSLCSDK_NUPKG:-}
if [ -z "$pkg" ]; then
    pkg=$work/package.nupkg
    curl -fsSL -o "$pkg" \
        "https://api.nuget.org/v3-flatcontainer/microsoft.wsl.containers/$version/microsoft.wsl.containers.$version.nupkg"
fi

require_sum "$pkg" "$nupkg_sha256" "the $version package"
unzip -p "$pkg" "runtimes/win-$arch/native/wslcsdk.dll" >"$work/wslcsdk.dll"
require_sum "$work/wslcsdk.dll" "$dll_sha256" "the $arch DLL"

mkdir -p "$dest"
cp "$work/wslcsdk.dll" "$dest/wslcsdk.dll"
echo "wslcsdk.dll $version ($arch) in $dest"
