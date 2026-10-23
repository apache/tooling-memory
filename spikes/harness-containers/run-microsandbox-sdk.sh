#!/bin/sh
# microsandbox through its Rust SDK (`msb-probe/`), for `disable_network()`, which the CLI
# cannot express: the network switched off rather than denied by policy. Otherwise as
# run-microsandbox.sh. MSB_PROBE names the built probe (`cargo build --release` in
# msb-probe/, outside the workspace); MSB_HOME as for msb. Arguments, if any, replace the
# check.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/fixture.sh"
set +e
[ $# -gt 0 ] || set -- python3 -I /work/contract-check.py
"${MSB_PROBE:?set MSB_PROBE to the built msb-probe}" "$FIX" \
    "${IMAGE:-docker.io/library/python:3.12-slim}" "$@"
echo "exit=$?"
