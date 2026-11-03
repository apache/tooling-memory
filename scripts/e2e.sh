#!/bin/sh
# The browser tests (e2e/) in the Playwright image, which carries Node, the browser and
# the libraries it needs, so the host needs neither. amem is built on the host first and
# runs in the container, whose base is the same Ubuntu as the image's; the same build
# gives the hosted copy of the pages, in target/hosted, that the tests serve as plain files.
#   scripts/e2e.sh                     the whole suite
#   scripts/e2e.sh tests/launch.spec.ts -g 'preview'
set -eu
here=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=mcr.microsoft.com/playwright:v1.61.1-noble
# target/hosted is this script's own output, in the build directory. A CI cache can
# bring back a copy from before hosted.sh marked what it makes, which hosted.sh would
# refuse to replace, so the run starts from an empty one.
rm -rf "$here/target/hosted"
sh "$here/scripts/hosted.sh" "$here/target/hosted"
if command -v podman >/dev/null 2>&1; then
    runtime=podman
    # files the run writes (node_modules, screenshots) belong to the caller
    user="--userns=keep-id"
else
    runtime=docker
    user="--user=$(id -u):$(id -g)"
fi
exec "$runtime" run --rm --init --network host --ipc host $user \
    -e HOME=/tmp -e CI="${CI:-}" -e AMEM=/work/target/debug/amem \
    -v "$here:/work" -w /work/e2e "$image" \
    sh -c 'npm ci --no-audit --no-fund --loglevel=error && npx tsc --noEmit && npx playwright test "$@"' e2e "$@"
