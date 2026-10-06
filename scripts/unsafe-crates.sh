#!/bin/sh
# The workspace forbids `unsafe`, and a crate escapes that only by leaving out
# `[lints] workspace = true` for a lint table of its own. AGENTS.md names the two crates
# that may: this fails when any other crate does, or when either of the two stops using
# its own table without the rule being changed.
#
# Run by `make check`.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "$here/.." && pwd)
allowed="sandbox wslc-api"

failures=0
found=0
for manifest in "$root"/crates/*/Cargo.toml; do
    found=$((found + 1))
    crate=$(basename "$(dirname "$manifest")")
    # a `workspace = true` line in the `[lints]` table, before the next table
    inherits=$(awk '/^\[/ { in_lints = ($0 == "[lints]") } in_lints && /^workspace *= *true/ { print "yes" }' "$manifest")
    case " $allowed " in
    *" $crate "*)
        if [ -n "$inherits" ]; then
            echo "crates/$crate inherits the workspace lints but AGENTS.md lists it as a crate that may use unsafe; change both together" >&2
            failures=$((failures + 1))
        fi
        ;;
    *)
        if [ -z "$inherits" ]; then
            echo "crates/$crate does not inherit the workspace lints, so unsafe is not forbidden there (AGENTS.md: only $allowed may use it)" >&2
            failures=$((failures + 1))
        fi
        ;;
    esac
done

if [ "$found" -eq 0 ]; then
    echo "unsafe-crates.sh: no crates found under $root/crates" >&2
    exit 1
fi
if [ "$failures" -ne 0 ]; then
    exit 1
fi
echo "unsafe-crates.sh: $found crates, unsafe only in $allowed"
